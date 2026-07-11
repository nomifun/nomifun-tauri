use std::{
    ffi::c_void,
    io,
    mem,
    ptr,
};

use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE},
    System::Threading::{
        DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, UpdateProcThreadAttribute,
    },
};

/// Exclusive ownership of one valid Win32 kernel handle.
#[derive(Debug)]
pub(super) struct OwnedHandle {
    handle: HANDLE,
}

impl OwnedHandle {
    /// Takes ownership of `handle`.
    ///
    /// # Safety
    ///
    /// The caller must transfer the sole responsibility for closing `handle`
    /// to the returned value. The handle must not be closed through any other
    /// alias while this value is alive.
    pub(super) unsafe fn from_raw(handle: HANDLE) -> io::Result<Self> {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }

        Ok(Self { handle })
    }

    pub(super) const fn as_raw(&self) -> HANDLE {
        self.handle
    }
}

// Win32 kernel handles may be used from any thread. `OwnedHandle` exposes no
// operation that can close the handle through a shared reference, and Drop
// requires exclusive ownership.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: construction rejects sentinel values and uniquely transfers
        // the responsibility to close the handle into this value.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

/// Owns one initialized process/thread attribute list and its attribute data.
pub(super) struct ProcThreadAttributeList {
    // `usize` supplies the pointer alignment required by the opaque Win32
    // structure while still allowing the probe result to be measured in bytes.
    storage: Box<[usize]>,
    // UpdateProcThreadAttribute retains this pointer until CreateProcessW uses
    // the list, so the backing array must live as long as the attribute list.
    _handle_list: Option<Box<[HANDLE]>>,
    // Records that the single pseudoconsole attribute has been installed.
    _pseudoconsole: Option<isize>,
}

impl ProcThreadAttributeList {
    /// Allocates and initializes space for exactly one attribute.
    pub(super) fn new_one() -> io::Result<Self> {
        Self::new(1)
    }

    pub(super) fn new(attribute_count: u32) -> io::Result<Self> {
        if attribute_count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "process attribute list must reserve at least one attribute",
            ));
        }
        let mut bytes_required = 0usize;

        // The sizing call normally fails with ERROR_INSUFFICIENT_BUFFER while
        // reporting the required allocation size.
        let probe_result = unsafe {
            InitializeProcThreadAttributeList(
                ptr::null_mut(),
                attribute_count,
                0,
                &mut bytes_required,
            )
        };
        if bytes_required == 0 {
            return if probe_result == 0 {
                Err(io::Error::last_os_error())
            } else {
                Err(io::Error::other(
                    "InitializeProcThreadAttributeList returned a zero allocation size",
                ))
            };
        }

        let word_size = mem::size_of::<usize>();
        let word_count = bytes_required
            .checked_add(word_size - 1)
            .ok_or_else(|| io::Error::other("process attribute allocation size overflow"))?
            / word_size;

        let mut words = Vec::new();
        words.try_reserve_exact(word_count).map_err(|error| {
            io::Error::other(format!(
                "failed to allocate process attribute list: {error}"
            ))
        })?;
        words.resize(word_count, 0usize);
        let mut storage = words.into_boxed_slice();

        let list = storage.as_mut_ptr().cast::<c_void>();
        let initialized = unsafe {
            InitializeProcThreadAttributeList(list, attribute_count, 0, &mut bytes_required)
        };
        if initialized == 0 {
            // The list was not initialized, so DeleteProcThreadAttributeList
            // must not be called. `storage` is ordinary Rust memory and drops
            // normally on this path.
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            storage,
            _handle_list: None,
            _pseudoconsole: None,
        })
    }

    /// Sets the exact set of inheritable handles visible to the child.
    pub(super) fn set_handle_list(&mut self, handles: &[HANDLE]) -> io::Result<()> {
        if handles.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the inherited handle whitelist must not be empty",
            ));
        }
        if self._handle_list.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "the inherited handle whitelist is already set",
            ));
        }
        if handles
            .iter()
            .any(|handle| handle.is_null() || *handle == INVALID_HANDLE_VALUE)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the inherited handle whitelist contains an invalid handle",
            ));
        }

        let handle_bytes = handles
            .len()
            .checked_mul(mem::size_of::<HANDLE>())
            .ok_or_else(|| io::Error::other("inherited handle list size overflow"))?;
        let owned_handles = handles.to_vec().into_boxed_slice();

        let updated = unsafe {
            UpdateProcThreadAttribute(
                self.as_mut_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                owned_handles.as_ptr().cast::<c_void>(),
                handle_bytes,
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if updated == 0 {
            return Err(io::Error::last_os_error());
        }

        self._handle_list = Some(owned_handles);
        Ok(())
    }

    pub(super) fn set_pseudoconsole(&mut self, pseudoconsole: isize) -> io::Result<()> {
        if pseudoconsole == 0 || pseudoconsole == -1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the pseudoconsole handle is invalid",
            ));
        }
        if self._pseudoconsole.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "the pseudoconsole attribute is already set",
            ));
        }

        let updated = unsafe {
            UpdateProcThreadAttribute(
                self.as_mut_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                pseudoconsole as *const c_void,
                mem::size_of::<isize>(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if updated == 0 {
            return Err(io::Error::last_os_error());
        }

        self._pseudoconsole = Some(pseudoconsole);
        Ok(())
    }

    pub(super) fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_mut_ptr().cast::<c_void>()
    }
}

impl Drop for ProcThreadAttributeList {
    fn drop(&mut self) {
        // SAFETY: `new_one` constructs this value only after successful
        // initialization. The attribute backing data remains alive throughout
        // this call and is released only after the Drop implementation returns.
        unsafe { DeleteProcThreadAttributeList(self.as_mut_ptr()) };
    }
}
