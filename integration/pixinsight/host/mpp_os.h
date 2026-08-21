#pragma once
// mpp_os.h -- minimal OS handle/IO/spawn seam so the transport code is
// written once. POSIX uses int fds; Windows uses HANDLEs. Everything else
// stays portable.
//
// The abstraction is deliberately tiny: a handle type (`os_handle`), an
// invalid sentinel (`os_invalid_handle`), blocking `os_read`/`os_write`/
// `os_close` primitives, a `Pipe` RAII pair, and an `os_spawn`/
// `os_wait_process`/`os_kill_process` child-process seam. Higher-level
// framing (mpp_protocol) and the WriteSession driver (mpp_host, Task 3) are
// written against these so the wire format and control flow are shared
// across platforms; only the raw byte IO/process primitives differ.
//
// NO PCL HEADER MAY EVER BE INCLUDED HERE (clean-room rule: this file and
// everything in integration/pixinsight/host/ must build and test without
// PixInsight installed).
//
// Divergence from MergeMosaic's mmm_os.h (see task-2 report for the full
// list): MMM keeps its spawn/pipe helpers private inside mmm_host.cpp,
// alongside its `Host` driver. mpp_os.h folds an equivalent `Pipe` +
// `os_spawn`/`os_wait_process`/`os_kill_process` seam directly into this
// header (as inline functions, no new .cpp unit) because Task 2's golden
// test needs to spawn the real worker before mpp_host.cpp exists (that
// lands in Task 3); Task 3's WriteSession driver reuses these primitives
// instead of re-implementing them.
#include <cstddef>
#include <stdexcept>
#include <string>
#include <vector>

#ifdef _WIN32
  #ifndef WIN32_LEAN_AND_MEAN
    #define WIN32_LEAN_AND_MEAN
  #endif
  #include <windows.h>
namespace mpp {
/// Platform handle type: a Win32 `HANDLE`.
using os_handle = HANDLE;
/// Sentinel for an unset/closed handle (matches Win32 conventions).
inline const os_handle os_invalid_handle = INVALID_HANDLE_VALUE;

/// Blocking read of up to `n` bytes; returns bytes read, 0 on EOF (peer
/// closed the pipe), or -1 on a hard error. A closed peer surfaces as
/// `ERROR_BROKEN_PIPE`, which we map to EOF to match POSIX `read()` returning
/// 0 at end of stream.
inline long os_read(os_handle h, void* buf, size_t n) {
    DWORD got = 0;
    if (!ReadFile(h, buf, (DWORD)n, &got, nullptr)) {
        if (GetLastError() == ERROR_BROKEN_PIPE) return 0;  // peer closed == EOF
        return -1;
    }
    return (long)got;
}

/// Blocking write of up to `n` bytes; returns bytes written or -1 on error.
/// A broken pipe (`ERROR_BROKEN_PIPE`/`ERROR_NO_DATA`) surfaces as -1, which
/// the caller maps to the same clean-error path POSIX takes on EPIPE.
inline long os_write(os_handle h, const void* buf, size_t n) {
    DWORD put = 0;
    if (!WriteFile(h, buf, (DWORD)n, &put, nullptr)) {
        return -1;  // includes ERROR_BROKEN_PIPE/ERROR_NO_DATA (== EPIPE)
    }
    return (long)put;
}

/// Close a handle, tolerating the invalid/null sentinels.
inline void os_close(os_handle h) {
    if (h && h != os_invalid_handle) CloseHandle(h);
}

namespace detail {
// Formats the current GetLastError() as a human string for diagnostics.
inline std::string last_error() {
    DWORD e = GetLastError();
    char* msg = nullptr;
    DWORD n = FormatMessageA(
        FORMAT_MESSAGE_ALLOCATE_BUFFER | FORMAT_MESSAGE_FROM_SYSTEM |
            FORMAT_MESSAGE_IGNORE_INSERTS,
        nullptr, e, 0, reinterpret_cast<char*>(&msg), 0, nullptr);
    std::string out = "os error " + std::to_string(e);
    if (n && msg) {
        out += ": ";
        out.append(msg, n);
        LocalFree(msg);
    }
    return out;
}

// Widen a UTF-8 narrow string to UTF-16 for the Win32 *W APIs.
inline std::wstring widen(const std::string& s) {
    if (s.empty()) return std::wstring();
    int n = MultiByteToWideChar(CP_UTF8, 0, s.data(), static_cast<int>(s.size()), nullptr, 0);
    std::wstring w(static_cast<size_t>(n), L'\0');
    MultiByteToWideChar(CP_UTF8, 0, s.data(), static_cast<int>(s.size()), w.data(), n);
    return w;
}

// Appends `arg` to `cmd` using the standard Win32 command-line quoting rules,
// quoting only when the arg is empty or contains whitespace/quotes.
inline void append_win_arg(std::wstring& cmd, const std::wstring& arg) {
    bool need_quote = arg.empty() || arg.find_first_of(L" \t\n\v\"") != std::wstring::npos;
    if (!need_quote) {
        cmd += arg;
        return;
    }
    cmd += L'"';
    for (size_t i = 0; i < arg.size();) {
        size_t backslashes = 0;
        while (i < arg.size() && arg[i] == L'\\') {
            ++i;
            ++backslashes;
        }
        if (i == arg.size()) {
            cmd.append(backslashes * 2, L'\\');
            break;
        } else if (arg[i] == L'"') {
            cmd.append(backslashes * 2 + 1, L'\\');
            cmd += L'"';
            ++i;
        } else {
            cmd.append(backslashes, L'\\');
            cmd += arg[i];
            ++i;
        }
    }
    cmd += L'"';
}

// Builds a mutable UTF-16 command line for CreateProcessW: "<exe>" <args...>.
inline std::wstring build_command_line_w(const std::string& exe_path,
                                          const std::vector<std::string>& args) {
    std::wstring cmd;
    cmd += L'"';
    cmd += widen(exe_path);
    cmd += L'"';
    for (const std::string& a : args) {
        cmd += L' ';
        append_win_arg(cmd, widen(a));
    }
    return cmd;
}
}  // namespace detail

/// A pipe pair; `read_end`/`write_end` are `os_invalid_handle` once closed.
/// Both ends are inheritable on Windows (per Win32 `CreatePipe` convention);
/// callers spawning a child mark the parent-retained end non-inheritable via
/// `SetHandleInformation` before spawning so only the child-side end is
/// inherited (essential for correct EOF propagation on Windows).
struct Pipe {
    os_handle read_end = os_invalid_handle;
    os_handle write_end = os_invalid_handle;

    Pipe() {
        SECURITY_ATTRIBUTES sa;
        sa.nLength = sizeof(sa);
        sa.bInheritHandle = TRUE;
        sa.lpSecurityDescriptor = nullptr;
        if (!CreatePipe(&read_end, &write_end, &sa, 0)) {
            throw std::runtime_error(std::string("CreatePipe failed: ") + detail::last_error());
        }
    }
    Pipe(const Pipe&) = delete;
    Pipe& operator=(const Pipe&) = delete;
    void close_read() {
        if (read_end != os_invalid_handle) {
            os_close(read_end);
            read_end = os_invalid_handle;
        }
    }
    void close_write() {
        if (write_end != os_invalid_handle) {
            os_close(write_end);
            write_end = os_invalid_handle;
        }
    }
    ~Pipe() {
        close_read();
        close_write();
    }
};

/// Platform child-process handle: a Win32 process `HANDLE`.
using os_pid = HANDLE;
/// Sentinel for "no process".
inline const os_pid os_invalid_pid = nullptr;

/// Spawns `exe_path` (no extra args -- `mpp-ipc-worker` takes none in normal
/// operation) with its stdin/stdout wired to the given (already child-side)
/// pipe ends; stderr is inherited from this process. `bInheritHandles = TRUE`
/// is required for `STARTF_USESTDHANDLES` to take effect; the caller must
/// have already made its own parent-retained pipe ends non-inheritable via
/// `SetHandleInformation` so the child does not keep them open (which would
/// prevent EOF). Throws `std::runtime_error` on failure. `CREATE_NO_WINDOW`
/// suppresses the console window Windows would otherwise pop for a console
/// app spawned from a GUI host (PixInsight).
inline os_pid os_spawn(const std::string& exe_path, os_handle child_stdin,
                        os_handle child_stdout) {
    STARTUPINFOW si;
    ZeroMemory(&si, sizeof si);
    si.cb = sizeof si;
    si.dwFlags = STARTF_USESTDHANDLES;
    si.hStdInput = child_stdin;
    si.hStdOutput = child_stdout;
    si.hStdError = GetStdHandle(STD_ERROR_HANDLE);

    PROCESS_INFORMATION pi;
    ZeroMemory(&pi, sizeof pi);

    std::wstring cmd = detail::build_command_line_w(exe_path, {});
    BOOL ok = CreateProcessW(nullptr, cmd.data(), nullptr, nullptr, TRUE /*inherit handles*/,
                              CREATE_NO_WINDOW, nullptr, nullptr, &si, &pi);
    if (!ok) {
        throw std::runtime_error("CreateProcessW(" + exe_path + ") failed: " + detail::last_error());
    }
    CloseHandle(pi.hThread);
    return pi.hProcess;
}

/// Blocks until `pid` exits; returns its exit code. Closes the process
/// handle before returning (the caller must not use `pid` afterward).
inline int os_wait_process(os_pid pid) {
    WaitForSingleObject(pid, INFINITE);
    DWORD code = 1;
    GetExitCodeProcess(pid, &code);
    CloseHandle(pid);
    return static_cast<int>(code);
}

/// Forcibly terminates `pid`. Safe to call on an already-exited process
/// (best effort; errors are ignored -- the caller still must reap via
/// `os_wait_process`).
inline void os_kill_process(os_pid pid) { TerminateProcess(pid, 1); }

/// Waits up to `timeout_ms` for `h` (a pipe read end) to have data (or EOF /
/// error) available for a non-blocking-ish `os_read`. Returns 1 when a read
/// would make progress, 0 on timeout. Anonymous pipe handles cannot be waited
/// on directly on Windows, so this polls `PeekNamedPipe` (which does work on
/// anonymous pipes) in short sleeps; a broken pipe reports 1 so the caller's
/// read observes the EOF.
inline int os_wait_readable(os_handle h, int timeout_ms) {
    for (int waited = 0;;) {
        DWORD avail = 0;
        if (!PeekNamedPipe(h, nullptr, 0, nullptr, &avail, nullptr))
            return 1;  // broken/closed pipe: let the read surface EOF/error
        if (avail > 0) return 1;
        if (waited >= timeout_ms) return 0;
        DWORD slice = (DWORD)((timeout_ms - waited) < 10 ? (timeout_ms - waited) : 10);
        Sleep(slice);
        waited += (int)slice;
    }
}
}  // namespace mpp
#else
  #include <cerrno>
  #include <csignal>
  #include <cstring>
  #include <poll.h>
  #include <spawn.h>
  #include <sys/wait.h>
  #include <unistd.h>

extern char** environ;

namespace mpp {
/// Platform handle type: a POSIX file descriptor.
using os_handle = int;
/// Sentinel for an unset/closed descriptor.
inline constexpr os_handle os_invalid_handle = -1;

/// Blocking read; returns bytes read, 0 on EOF, or -1 on error (errno set).
inline long os_read(os_handle h, void* buf, size_t n) { return (long)::read(h, buf, n); }
/// Blocking write; returns bytes written or -1 on error (errno set).
inline long os_write(os_handle h, const void* buf, size_t n) { return (long)::write(h, buf, n); }
/// Close a descriptor, tolerating the invalid sentinel.
inline void os_close(os_handle h) { if (h >= 0) ::close(h); }

/// A pipe pair; `read_end`/`write_end` are `os_invalid_handle` once closed.
struct Pipe {
    os_handle read_end = os_invalid_handle;
    os_handle write_end = os_invalid_handle;

    Pipe() {
        int fd[2];
        if (::pipe(fd) != 0) {
            throw std::runtime_error(std::string("pipe: ") + std::strerror(errno));
        }
        read_end = fd[0];
        write_end = fd[1];
    }
    Pipe(const Pipe&) = delete;
    Pipe& operator=(const Pipe&) = delete;
    void close_read() {
        if (read_end != os_invalid_handle) {
            os_close(read_end);
            read_end = os_invalid_handle;
        }
    }
    void close_write() {
        if (write_end != os_invalid_handle) {
            os_close(write_end);
            write_end = os_invalid_handle;
        }
    }
    ~Pipe() {
        close_read();
        close_write();
    }
};

/// Platform child-process handle: a POSIX pid.
using os_pid = pid_t;
/// Sentinel for "no process".
inline constexpr os_pid os_invalid_pid = -1;

namespace detail {
// Ignore SIGPIPE process-wide, once, so a write to the stdin of a worker
// that has just died (its read end fully closed) fails with EPIPE instead
// of delivering the default-fatal SIGPIPE that would terminate the whole
// host. `std::call_once`-equivalent via a static bool + a plain guard is
// unnecessary here since `signal()` itself is idempotent to call repeatedly;
// the flag just avoids the (harmless) repeated syscall.
inline void ignore_sigpipe_once() {
    static bool done = false;
    if (!done) {
        ::signal(SIGPIPE, SIG_IGN);
        done = true;
    }
}
}  // namespace detail

/// Spawns `exe_path` (no extra args -- `mpp-ipc-worker` takes none in normal
/// operation) via `posix_spawn`, redirecting the child's stdin/stdout to the
/// given (already child-side) pipe descriptors; stderr is inherited. The
/// caller retains ownership of `child_stdin`/`child_stdout` (still open in
/// this process after the call) and of every other open pipe descriptor --
/// close them explicitly after spawning, same as `posix_spawn`'s usual
/// contract. Throws `std::runtime_error` on failure.
inline os_pid os_spawn(const std::string& exe_path, os_handle child_stdin,
                        os_handle child_stdout) {
    detail::ignore_sigpipe_once();

    posix_spawn_file_actions_t fa;
    posix_spawn_file_actions_init(&fa);
    posix_spawn_file_actions_adddup2(&fa, child_stdin, STDIN_FILENO);
    posix_spawn_file_actions_adddup2(&fa, child_stdout, STDOUT_FILENO);
    posix_spawn_file_actions_addclose(&fa, child_stdin);
    posix_spawn_file_actions_addclose(&fa, child_stdout);

    std::string exe_copy = exe_path;
    char* argv[] = {exe_copy.data(), nullptr};

    pid_t pid = -1;
    int rc = posix_spawn(&pid, exe_path.c_str(), &fa, nullptr, argv, environ);
    posix_spawn_file_actions_destroy(&fa);
    if (rc != 0) {
        throw std::runtime_error("posix_spawn " + exe_path + ": " + std::strerror(rc));
    }
    return pid;
}

/// Blocks until `pid` exits; returns its exit code (0..255) if it exited
/// normally, or `128 + signal_number` if it was terminated by a signal --
/// the conventional POSIX shell encoding, unambiguous versus a normal exit
/// code (which cannot itself reach 128 for `mpp-ipc-worker`'s own exit
/// codes, 0 or 1).
inline int os_wait_process(os_pid pid) {
    int status = 0;
    for (;;) {
        if (::waitpid(pid, &status, 0) >= 0) break;
        if (errno != EINTR) break;
    }
    if (WIFEXITED(status)) return WEXITSTATUS(status);
    if (WIFSIGNALED(status)) return 128 + WTERMSIG(status);
    return 1;
}

/// Forcibly terminates `pid` (SIGKILL). Safe to call on an already-exited
/// process (best effort; errors are ignored -- the caller still must reap
/// via `os_wait_process`).
inline void os_kill_process(os_pid pid) { ::kill(pid, SIGKILL); }

/// Waits up to `timeout_ms` for `h` to be readable (data, EOF, or error).
/// Returns 1 when a read would make progress, 0 on timeout. A `poll()` error
/// (other than EINTR, which retries) reports 1 so the caller's read surfaces
/// the real errno.
inline int os_wait_readable(os_handle h, int timeout_ms) {
    struct pollfd pfd;
    pfd.fd = h;
    pfd.events = POLLIN;
    pfd.revents = 0;
    for (;;) {
        int r = ::poll(&pfd, 1, timeout_ms);
        if (r > 0) return 1;   // readable, EOF (POLLHUP), or error (POLLERR)
        if (r == 0) return 0;  // timeout
        if (errno != EINTR) return 1;  // let the read surface the error
    }
}
}  // namespace mpp
#endif
