// Copyright © 2019 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0
//

use arc_swap::ArcSwap;
use arch::RegionType;
use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::*;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::FromRawFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use vm_allocator::SystemAllocator;
use vm_memory::guest_memory::FileOffset;
use vm_memory::{
    Address, Error as MmapError, GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion,
    GuestUsize,
};

pub struct MemoryManager {
    guest_memory: Arc<ArcSwap<GuestMemoryMmap>>,
    next_kvm_memory_slot: u32,
    start_of_device_area: GuestAddress,
    end_of_device_area: GuestAddress,
    fd: Arc<VmFd>,
}

#[derive(Debug)]
pub enum Error {
    /// Failed to create shared file.
    SharedFileCreate(io::Error),

    /// Failed to set shared file length.
    SharedFileSetLen(io::Error),

    /// Mmap backed guest memory error
    GuestMemory(MmapError),

    /// Failed to allocate a memory range.
    MemoryRangeAllocation,
}

pub fn get_host_cpu_phys_bits() -> u8 {
    use core::arch::x86_64;
    unsafe {
        let leaf = x86_64::__cpuid(0x8000_0000);

        // Detect and handle AMD SME (Secure Memory Encryption) properly.
        // Some physical address bits may become reserved when the feature is enabled.
        // See AMD64 Architecture Programmer's Manual Volume 2, Section 7.10.1
        let reduced = if leaf.eax >= 0x8000_001f
            && leaf.ebx == 0x6874_7541    // Vendor ID: AuthenticAMD
            && leaf.ecx == 0x444d_4163
            && leaf.edx == 0x6974_6e65
            && x86_64::__cpuid(0x8000_001f).eax & 0x1 != 0
        {
            (x86_64::__cpuid(0x8000_001f).ebx >> 6) & 0x3f
        } else {
            0
        };

        if leaf.eax >= 0x8000_0008 {
            let leaf = x86_64::__cpuid(0x8000_0008);
            ((leaf.eax & 0xff) - reduced) as u8
        } else {
            36
        }
    }
}

impl MemoryManager {
    pub fn new(
        allocator: Arc<Mutex<SystemAllocator>>,
        fd: Arc<VmFd>,
        boot_ram: u64,
        backing_file: &Option<PathBuf>,
        mergeable: bool,
    ) -> Result<Arc<Mutex<MemoryManager>>, Error> {
        // Init guest memory
        let arch_mem_regions = arch::arch_memory_regions(boot_ram);

        let ram_regions: Vec<(GuestAddress, usize)> = arch_mem_regions
            .iter()
            .filter(|r| r.2 == RegionType::Ram)
            .map(|r| (r.0, r.1))
            .collect();

        let guest_memory = match backing_file {
            Some(ref file) => {
                let mut mem_regions = Vec::<(GuestAddress, usize, Option<FileOffset>)>::new();
                for region in ram_regions.iter() {
                    if file.is_file() {
                        let file = OpenOptions::new()
                            .read(true)
                            .write(true)
                            .open(file)
                            .map_err(Error::SharedFileCreate)?;

                        file.set_len(region.1 as u64)
                            .map_err(Error::SharedFileSetLen)?;

                        mem_regions.push((region.0, region.1, Some(FileOffset::new(file, 0))));
                    } else if file.is_dir() {
                        let fs_str = format!("{}{}", file.display(), "/tmpfile_XXXXXX");
                        let fs = std::ffi::CString::new(fs_str).unwrap();
                        let mut path = fs.as_bytes_with_nul().to_owned();
                        let path_ptr = path.as_mut_ptr() as *mut _;
                        let fd = unsafe { libc::mkstemp(path_ptr) };
                        unsafe { libc::unlink(path_ptr) };

                        let f = unsafe { File::from_raw_fd(fd) };
                        f.set_len(region.1 as u64)
                            .map_err(Error::SharedFileSetLen)?;

                        mem_regions.push((region.0, region.1, Some(FileOffset::new(f, 0))));
                    }
                }

                GuestMemoryMmap::with_files(&mem_regions).map_err(Error::GuestMemory)?
            }
            None => GuestMemoryMmap::new(&ram_regions).map_err(Error::GuestMemory)?,
        };

        let end_of_device_area = GuestAddress((1 << get_host_cpu_phys_bits()) - 1);
        let mem_end = guest_memory.end_addr();
        let start_of_device_area = if mem_end < arch::layout::MEM_32BIT_RESERVED_START {
            arch::layout::RAM_64BIT_START
        } else {
            mem_end.unchecked_add(1)
        };

        let guest_memory = Arc::new(ArcSwap::new(Arc::new(guest_memory)));

        let memory_manager = Arc::new(Mutex::new(MemoryManager {
            guest_memory: guest_memory.clone(),
            next_kvm_memory_slot: ram_regions.len() as u32,
            start_of_device_area,
            end_of_device_area,
            fd,
        }));

        guest_memory.load().with_regions(|_, region| {
            let _ = memory_manager.lock().unwrap().create_userspace_mapping(
                region.start_addr().raw_value(),
                region.len() as u64,
                region.as_ptr() as u64,
                mergeable,
            )?;
            Ok(())
        })?;

        // Allocate RAM and Reserved address ranges.
        for region in arch_mem_regions.iter() {
            allocator
                .lock()
                .unwrap()
                .allocate_mmio_addresses(Some(region.0), region.1 as GuestUsize, None)
                .ok_or(Error::MemoryRangeAllocation)?;
        }

        Ok(memory_manager)
    }

    pub fn guest_memory(&self) -> Arc<ArcSwap<GuestMemoryMmap>> {
        self.guest_memory.clone()
    }

    pub fn start_of_device_area(&self) -> GuestAddress {
        self.start_of_device_area
    }

    pub fn end_of_device_area(&self) -> GuestAddress {
        self.end_of_device_area
    }

    pub fn allocate_kvm_memory_slot(&mut self) -> u32 {
        let slot_id = self.next_kvm_memory_slot;
        self.next_kvm_memory_slot += 1;
        slot_id
    }

    pub fn create_userspace_mapping(
        &mut self,
        guest_phys_addr: u64,
        memory_size: u64,
        userspace_addr: u64,
        mergeable: bool,
    ) -> Result<u32, Error> {
        let slot = self.allocate_kvm_memory_slot();
        let mem_region = kvm_userspace_memory_region {
            slot,
            guest_phys_addr,
            memory_size,
            userspace_addr,
            flags: 0,
        };

        // Safe because the guest regions are guaranteed not to overlap.
        unsafe {
            self.fd
                .set_user_memory_region(mem_region)
                .map_err(|e| io::Error::from_raw_os_error(e.errno()))
        }
        .map_err(|_: io::Error| Error::GuestMemory(MmapError::NoMemoryRegion))?;

        // Mark the pages as mergeable if explicitly asked for.
        if mergeable {
            // Safe because the address and size are valid since the
            // mmap succeeded.
            let ret = unsafe {
                libc::madvise(
                    userspace_addr as *mut libc::c_void,
                    memory_size as libc::size_t,
                    libc::MADV_MERGEABLE,
                )
            };
            if ret != 0 {
                let err = io::Error::last_os_error();
                // Safe to unwrap because the error is constructed with
                // last_os_error(), which ensures the output will be Some().
                let errno = err.raw_os_error().unwrap();
                if errno == libc::EINVAL {
                    warn!("kernel not configured with CONFIG_KSM");
                } else {
                    warn!("madvise error: {}", err);
                }
                warn!("failed to mark pages as mergeable");
            }
        }

        Ok(slot)
    }
}
