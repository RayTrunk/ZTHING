//! The named pipe server, which holds every Win32 call of the service. Each
//! `unsafe` block passes handles and buffers that stay valid for the duration
//! of a synchronous call.

use std::io::{self, Read, Write};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, INVALID_HANDLE_VALUE,
    LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, RevertToSelf, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER, TokenUser,
};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FlushFileBuffers, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, ImpersonateNamedPipeClient,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
    PIPE_WAIT,
};
use windows::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};
use windows::core::{HSTRING, PWSTR};

/// Denies network logons, gives SYSTEM and administrators full access and lets
/// interactively logged-on users read and write. `P` blocks inherited entries.
const SECURITY: &str = "D:P(D;;GA;;;NU)(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)";
const BUFFER_SIZE: u32 = 64 * 1024;

pub struct Connection {
    handle: HANDLE,
}

// SAFETY: a pipe handle is valid in every thread of the process, and the
// connection is used by one thread at a time.
unsafe impl Send for Connection {}

/// Creates a pipe instance and waits for a client. With `first` set the call
/// fails if another process already owns the name, so nobody can pose as the service.
pub fn accept(name: &str, first: bool) -> io::Result<Connection> {
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the string outlives the call. The descriptor is freed below.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &HSTRING::from(SECURITY),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
    }
    .map_err(io::Error::other)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    let open_mode = if first {
        PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE
    } else {
        PIPE_ACCESS_DUPLEX
    };
    // SAFETY: name and attributes outlive the call. `Connection` owns the handle.
    let handle = unsafe {
        CreateNamedPipeW(
            &HSTRING::from(name),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            BUFFER_SIZE,
            BUFFER_SIZE,
            0,
            Some(&attributes),
        )
    };
    let created = if handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(Connection { handle })
    };
    // SAFETY: the descriptor came from the conversion above. The pipe keeps its own copy.
    unsafe { LocalFree(Some(HLOCAL(descriptor.0))) };
    let connection = created?;
    // SAFETY: synchronous call on the pipe handle created above.
    match unsafe { ConnectNamedPipe(connection.handle, None) } {
        Ok(()) => Ok(connection),
        // The client connected between CreateNamedPipeW and ConnectNamedPipe.
        Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => Ok(connection),
        Err(e) => Err(io::Error::other(e)),
    }
}

impl Connection {
    /// The client's SID such as `S-1-5-21-…`, which keys the per-user access cache.
    pub fn client_sid(&self) -> io::Result<String> {
        self.impersonate(thread_user_sid)?
    }

    /// Runs `f` with the access rights of the client. If reverting fails the
    /// error ends the connection thread, which takes the client identity with it.
    pub fn impersonate<T>(&self, f: impl FnOnce() -> T) -> io::Result<T> {
        // SAFETY: synchronous call on a connected pipe handle.
        unsafe { ImpersonateNamedPipeClient(self.handle) }.map_err(io::Error::other)?;
        let result = f();
        // SAFETY: takes no arguments and undoes the impersonation of this thread.
        unsafe { RevertToSelf() }.map_err(io::Error::other)?;
        Ok(result)
    }
}

impl Read for &Connection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut read = 0u32;
        // SAFETY: `buf` and `read` outlive the synchronous call.
        match unsafe { ReadFile(self.handle, Some(buf), Some(&mut read), None) } {
            Ok(()) => Ok(read as usize),
            // The client closed its end.
            Err(e) if e.code() == ERROR_BROKEN_PIPE.to_hresult() => Ok(0),
            Err(e) => Err(io::Error::other(e)),
        }
    }
}

impl Write for &Connection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut written = 0u32;
        // SAFETY: `buf` and `written` outlive the synchronous call.
        unsafe { WriteFile(self.handle, Some(buf), Some(&mut written), None) }
            .map_err(io::Error::other)?;
        Ok(written as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateNamedPipeW and is closed exactly once.
        // Errors are ignored because the client may already be gone.
        unsafe {
            let _ = FlushFileBuffers(self.handle);
            let _ = DisconnectNamedPipe(self.handle);
            let _ = CloseHandle(self.handle);
        }
    }
}

/// SID of the user the current thread acts for. Called while impersonating.
fn thread_user_sid() -> io::Result<String> {
    let mut token = HANDLE::default();
    // SAFETY: `token` outlives the call and is closed below. Opening as self
    // checks the service's own rights, which may read its threads' tokens.
    unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &mut token) }
        .map_err(io::Error::other)?;
    // u64 elements give the 8-byte alignment that TOKEN_USER needs. 64 bytes
    // hold the largest SID plus the TOKEN_USER header.
    let mut buffer = [0u64; 16];
    let mut len = 0u32;
    // SAFETY: the buffer and `len` outlive the call, the size matches the buffer.
    let read = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            size_of_val(&buffer) as u32,
            &mut len,
        )
    };
    // SAFETY: the token came from OpenThreadToken and is closed exactly once.
    let _ = unsafe { CloseHandle(token) };
    read.map_err(io::Error::other)?;
    // SAFETY: GetTokenInformation filled the aligned buffer with a TOKEN_USER
    // whose SID pointer points into the same buffer.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut text = PWSTR::null();
    // SAFETY: the SID stays valid while `buffer` lives. `text` is freed below.
    unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) }.map_err(io::Error::other)?;
    // SAFETY: ConvertSidToStringSidW returned a null-terminated wide string.
    let sid = unsafe { text.to_string() }.map_err(io::Error::other);
    // SAFETY: the string came from ConvertSidToStringSidW, which allocates with LocalAlloc.
    unsafe { LocalFree(Some(HLOCAL(text.0.cast()))) };
    sid
}
