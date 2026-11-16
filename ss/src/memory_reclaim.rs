#[cfg(target_os = "macos")]
use std::ffi::c_void;
#[cfg(target_os = "macos")]
use std::ptr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReclaimResult {
    pub released_bytes: usize,
    pub supported: bool,
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn malloc_zone_pressure_relief(zone: *mut c_void, goal: usize) -> usize;
}

pub(crate) fn reclaim_free_pages() -> ReclaimResult {
    #[cfg(target_os = "macos")]
    {
        // null selects every zone and zero asks for maximal pressure relief, as declared by the macos sdk
        let released_bytes = unsafe { malloc_zone_pressure_relief(ptr::null_mut(), 0) };
        ReclaimResult {
            released_bytes,
            supported: true,
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        ReclaimResult {
            released_bytes: 0,
            supported: false,
        }
    }
}

