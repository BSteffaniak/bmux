// Standalone Win32 test tooling; build in a native MSVC tools shell (README).
#define _CRT_RAND_S
#define _WIN32_WINNT 0x0A00
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <algorithm>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <stdexcept>
#include <string>
#include <vector>
#include <windows.h>

using namespace std::chrono_literals;
using Clock = std::chrono::steady_clock;
using Bytes = std::vector<unsigned char>;
static void require(bool ok, const char *why) {
  if (!ok)
    throw std::runtime_error(why);
}
static void win(bool ok, const char *why) {
  if (!ok)
    throw std::runtime_error(std::string(why) + " (Win32 " +
                             std::to_string(GetLastError()) + ")");
}
static std::wstring objname(HANDLE h) {
  wchar_t name[128]{};
  DWORD used;
  win(GetUserObjectInformationW(h, UOI_NAME, name, sizeof name, &used) != 0,
      "UOI_NAME");
  return name;
}
static void verify(const std::wstring &expected) {
  require(expected.size() == 41 && expected.substr(0, 9) == L"BmuxTest_" &&
              expected.find_first_not_of(L"0123456789abcdef", 9) ==
                  std::wstring::npos,
          "invalid test name");
  auto station = GetProcessWindowStation();
  USEROBJECTFLAGS flags{};
  DWORD used;
  win(GetUserObjectInformationW(station, UOI_FLAGS, &flags, sizeof flags,
                                &used) != 0,
      "UOI_FLAGS");
  require(!(flags.dwFlags & WSF_VISIBLE) && objname(station) == expected &&
              objname(GetThreadDesktop(GetCurrentThreadId())) == L"BmuxDesk",
          "wrong station or desktop");
}
// The short-lived launcher is single threaded. Nothing calls SwitchDesktop or
// SetThreadDesktop; lpDesktop is set explicitly on every child. Handles are
// non-inheritable except the explicitly listed stdio endpoints.
struct Station {
  HWINSTA original = GetProcessWindowStation(), ws = nullptr;
  HDESK desk = nullptr;
  std::wstring name = L"BmuxTest_";
  Station() {
    for (int i = 0; i < 4; ++i) {
      unsigned v;
      require(rand_s(&v) == 0, "random station name");
      wchar_t hex[9];
      swprintf_s(hex, L"%08x", v);
      name += hex;
    }
    // CWF_CREATE_ONLY rejects collisions rather than opening an existing
    // station.
    ws = CreateWindowStationW(name.c_str(), CWF_CREATE_ONLY, WINSTA_ALL_ACCESS,
                              nullptr);
    win(ws != nullptr, "CreateWindowStation");
    win(SetProcessWindowStation(ws) != 0, "SetProcessWindowStation");
    desk =
        CreateDesktopW(L"BmuxDesk", nullptr, nullptr, 0, GENERIC_ALL, nullptr);
    DWORD err = GetLastError();
    win(SetProcessWindowStation(original) != 0, "restore launcher station");
    SetLastError(err);
    win(desk != nullptr, "CreateDesktop");
    require(objname(ws) == name && objname(desk) == L"BmuxDesk",
            "created objects mismatch");
  }
  ~Station() {
    CloseDesktop(desk);
    CloseWindowStation(ws);
  }
};
struct Handle {
  HANDLE h = nullptr;
  Handle() = default;
  Handle(const Handle &) = delete;
  Handle &operator=(const Handle &) = delete;
  void close() {
    if (h) {
      CloseHandle(h);
      h = nullptr;
    }
  }
  ~Handle() { close(); }
};
struct Child {
  Handle proc, input, output;
  Child(const Station &station, const std::wstring &exe,
        const std::wstring &args) {
    SECURITY_ATTRIBUTES sa{sizeof sa, nullptr, TRUE};
    Handle in, out;
    win(CreatePipe(&in.h, &input.h, &sa, 0) != 0, "stdin pipe");
    win(CreatePipe(&output.h, &out.h, &sa, 0) != 0, "stdout pipe");
    win(SetHandleInformation(input.h, HANDLE_FLAG_INHERIT, 0) &&
            SetHandleInformation(output.h, HANDLE_FLAG_INHERIT, 0),
        "private pipe ends");
    SIZE_T cb = 0;
    InitializeProcThreadAttributeList(nullptr, 1, 0, &cb);
    Bytes storage(cb);
    auto attrs = reinterpret_cast<LPPROC_THREAD_ATTRIBUTE_LIST>(storage.data());
    win(InitializeProcThreadAttributeList(attrs, 1, 0, &cb) != 0,
        "attribute list");
    struct FreeList {
      LPPROC_THREAD_ATTRIBUTE_LIST p;
      ~FreeList() { DeleteProcThreadAttributeList(p); }
    } freeList{attrs};
    HANDLE pass[] = {in.h, out.h};
    win(UpdateProcThreadAttribute(attrs, 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                                  pass, sizeof pass, nullptr, nullptr) != 0,
        "stdio handles");
    auto desktop = station.name + L"\\BmuxDesk";
    STARTUPINFOEXW si{};
    si.StartupInfo.cb = sizeof si;
    si.StartupInfo.lpDesktop = desktop.data();
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = in.h;
    si.StartupInfo.hStdOutput = si.StartupInfo.hStdError = out.h;
    si.lpAttributeList = attrs;
    auto command = L"\"" + exe + L"\" " + args;
    PROCESS_INFORMATION pi{};
    win(CreateProcessW(exe.c_str(), command.data(), nullptr, nullptr, TRUE,
                       EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW, nullptr,
                       nullptr, &si.StartupInfo, &pi) != 0,
        "CreateProcess");
    proc.h = pi.hProcess;
    CloseHandle(pi.hThread);
  }
  std::string read() {
    std::string result;
    for (;;) {
      DWORD n = 0;
      if (!PeekNamedPipe(output.h, nullptr, 0, nullptr, &n, nullptr)) {
        win(GetLastError() == ERROR_BROKEN_PIPE, "PeekNamedPipe");
        break;
      }
      if (!n)
        break;
      char buf[256];
      win(ReadFile(output.h, buf, std::min<DWORD>(n, sizeof buf), &n,
                   nullptr) != 0,
          "ReadFile");
      result.append(buf, n);
      require(result.size() <= 1024, "excess peer output");
    }
    return result;
  }
  void ready() {
    std::string s;
    auto end = Clock::now() + 15s;
    while (Clock::now() < end) {
      s += read();
      if (s == "ready\n" || s == "ready\r\n")
        return;
      require(s.size() < 8 && WaitForSingleObject(proc.h, 0) == WAIT_TIMEOUT,
              "writer failed before ready");
      Sleep(5);
    }
    throw std::runtime_error("writer ready timeout");
  }
  DWORD wait() {
    require(WaitForSingleObject(proc.h, 15000) == WAIT_OBJECT_0,
            "peer timeout");
    DWORD code;
    win(GetExitCodeProcess(proc.h, &code) != 0, "peer status");
    return code;
  }
  ~Child() {
    input.close();
    if (proc.h && WaitForSingleObject(proc.h, 0) == WAIT_TIMEOUT) {
      TerminateProcess(proc.h, 124);
      WaitForSingleObject(proc.h, 5000);
    }
  }
};

// Independent oracle: only USER32/KERNEL32. Request an explicit DIBV5, not
// the registered PNG representation that the BMUX reader normally prefers.
static Bytes fixture(const std::wstring &kind, unsigned seed) {
  if (kind == L"image") {
    BITMAPV5HEADER h{};
    h.bV5Size = sizeof h;
    h.bV5Width = 37;
    h.bV5Height = 29;
    h.bV5Planes = 1;
    h.bV5BitCount = 32;
    h.bV5Compression = BI_BITFIELDS;
    h.bV5SizeImage = 37 * 29 * 4;
    h.bV5RedMask = 0xff0000;
    h.bV5GreenMask = 0xff00;
    h.bV5BlueMask = 0xff;
    h.bV5AlphaMask = 0xff000000;
    h.bV5CSType = LCS_sRGB;
    h.bV5Intent = LCS_GM_IMAGES;
    Bytes b(sizeof h);
    memcpy(b.data(), &h, sizeof h);
    for (int y = 28; y >= 0; --y)
      for (int x = 0; x < 37; ++x) {
        unsigned p = seed ^ ((x + 37 * y) * 0x9e3779b9u);
        b.insert(b.end(), {static_cast<unsigned char>(p >> 16),
                           static_cast<unsigned char>(p >> 8),
                           static_cast<unsigned char>(p), 255});
      }
    return b;
  }
  require(kind == L"empty" || kind == L"text", "unknown fixture");
  auto line =
      L"bmux " + std::to_wstring(seed) + L" \u754c \U0001f980 e\u0301\n";
  std::wstring s;
  if (kind == L"text")
    for (int i = 0; i < 8192; ++i)
      s += line;
  Bytes b((s.size() + 1) * sizeof(wchar_t));
  memcpy(b.data(), s.c_str(), b.size());
  return b;
}
static void check_image(const unsigned char *p, size_t n,
                        const Bytes &expected) {
  BITMAPV5HEADER h{};
  require(n >= sizeof h, "short DIBV5");
  memcpy(&h, p, sizeof h);
  require(h.bV5Size == sizeof h && h.bV5Width == 37 &&
              (h.bV5Height == 29 || h.bV5Height == -29) && h.bV5Planes == 1 &&
              h.bV5BitCount == 32 && h.bV5Compression == BI_BITFIELDS &&
              h.bV5RedMask == 0xff0000 && h.bV5GreenMask == 0xff00 &&
              h.bV5BlueMask == 0xff && h.bV5AlphaMask == 0xff000000 &&
              !h.bV5ClrUsed && !h.bV5ProfileSize && n >= expected.size(),
          "unexpected DIBV5 layout");
  for (int y = 0; y < 29; ++y)
    require(!memcmp(p + sizeof h + (h.bV5Height > 0 ? y : 28 - y) * 148,
                    expected.data() + sizeof h + y * 148, 148),
            "DIBV5 pixel mismatch");
}
static void peer(const std::wstring &op, const std::wstring &kind,
                 unsigned seed) {
  wchar_t name[128];
  DWORD n = GetEnvironmentVariableW(L"BMUX_WIN_TEST_STATION", name, 128);
  require(n && n < 128, "missing station");
  verify(name);
  auto data = fixture(kind, seed);
  UINT fmt = kind == L"image" ? CF_DIBV5 : CF_UNICODETEXT;
  HWND w = CreateWindowExW(0, L"STATIC", L"", 0, 0, 0, 0, 0, HWND_MESSAGE,
                           nullptr, nullptr, nullptr);
  win(w != nullptr, "owner window");
  struct Window {
    HWND w;
    ~Window() { DestroyWindow(w); }
  } window{w};
  win(OpenClipboard(w) != 0, "OpenClipboard");
  struct Board {
    bool open = true;
    ~Board() {
      if (open)
        CloseClipboard();
    }
  } board;
  if (op == L"write") {
    win(EmptyClipboard() != 0, "EmptyClipboard");
    auto h = GlobalAlloc(GMEM_MOVEABLE, data.size());
    win(h != nullptr, "GlobalAlloc");
    struct Owned {
      HGLOBAL h;
      ~Owned() {
        if (h)
          GlobalFree(h);
      }
    } owned{h};
    auto dst = GlobalLock(h);
    win(dst != nullptr, "GlobalLock");
    memcpy(dst, data.data(), data.size());
    GlobalUnlock(h);
    win(SetClipboardData(fmt, h) != nullptr, "SetClipboardData");
    owned.h = nullptr;
  } else if (op == L"check") {
    auto h = static_cast<HGLOBAL>(GetClipboardData(fmt));
    win(h != nullptr, "GetClipboardData");
    auto size = GlobalSize(h);
    require(size >= data.size() && size <= (1u << 20),
            "unexpected allocation size");
    auto src = static_cast<unsigned char *>(GlobalLock(h));
    win(src != nullptr, "GlobalLock");
    struct Unlock {
      HGLOBAL h;
      ~Unlock() { GlobalUnlock(h); }
    } locked{h};
    if (kind == L"image")
      check_image(src, size, data);
    else
      require(!memcmp(src, data.data(), data.size()), "UTF-16 mismatch");
  } else
    require(op == L"lock", "unknown peer operation");
  if (op != L"lock") {
    win(CloseClipboard() != 0, "CloseClipboard");
    board.open = false;
  }
  if (op != L"check") {
    puts("ready");
    fflush(stdout);
    (void)getchar();
  }
}

static void probe(const Station &station, const std::wstring &exe) {
  Child p(station, exe, L"verify-only");
  require(p.wait() == 0, "guarded peer preflight failed");
  auto s = p.read();
  require(s == "verified\n" || s == "verified\r\n", "not a guarded peer");
}
static void matrix(const Station &station, const std::wstring &self,
                   const std::wstring &old, const std::wstring &candidate) {
  probe(station, self);
  probe(station, old);
  probe(station, candidate);
  struct Leg {
    const char *label;
    std::wstring w, r;
  };
  unsigned seed = 100;
  int failures = 0;
  for (auto &leg :
       {Leg{"shipping->win32", old, self}, Leg{"win32->shipping", self, old},
        Leg{"candidate->win32", candidate, self},
        Leg{"win32->candidate", self, candidate},
        Leg{"shipping->candidate", old, candidate},
        Leg{"candidate->shipping", candidate, old}}) {
    for (auto kind : {L"text", L"empty", L"image"}) {
      auto args = std::wstring(L" ") + kind + L" " + std::to_wstring(++seed);
      printf("%s %ls seed=%u ", leg.label, kind, seed);
      fflush(stdout);
      try {
        Child owner(station, leg.w, L"write" + args);
        owner.ready();
        auto check = [&] {
          Child reader(station, leg.r, L"check" + args);
          require(reader.wait() == 0, "reader rejected fixture");
        };
        check();
        require(WaitForSingleObject(owner.proc.h, 0) == WAIT_TIMEOUT,
                "writer exited early");
        printf("live=OK ");
        owner.input.close();
        require(owner.wait() == 0, "writer exit failed");
        check();
        puts("post-exit=OK");
      } catch (const std::exception &e) {
        printf("FAIL: %s\n", e.what());
        ++failures;
      }
    }
  }
  // A held board must fail within the peer deadline; after release a fresh
  // write must work. This does not infer the precise number of retries.
  Child locker(station, self, L"lock empty 0");
  locker.ready();
  Child blocked(station, candidate, L"write text 201");
  blocked.input.close();
  require(blocked.wait() == 1 &&
              blocked.read().find("text write failed") != std::string::npos,
          "unexpected locked-write result");
  locker.input.close();
  require(locker.wait() == 0, "lock release failed");
  Child retry(station, candidate, L"write text 202");
  retry.ready();
  retry.input.close();
  require(retry.wait() == 0, "retry failed");
  Child readback(station, self, L"check text 202");
  require(readback.wait() == 0, "retry content mismatch");
  puts("contention: held-write refused; released-write persisted");
  require(!failures, "interop failures above");
}
int wmain(int argc, wchar_t **argv) {
  try {
    if (argc == 2 && std::wstring(argv[1]) == L"verify-only") {
      wchar_t name[128];
      auto n = GetEnvironmentVariableW(L"BMUX_WIN_TEST_STATION", name, 128);
      require(n && n < 128, "no station");
      verify(name);
      puts("verified");
      return 0;
    }
    if (argc == 4 && std::wstring(argv[1]) == L"run") {
      Station station;
      wchar_t self[32768];
      auto n = GetModuleFileNameW(nullptr, self, 32768);
      win(n && n < 32768, "launcher path");
      win(SetEnvironmentVariableW(L"PATH", nullptr) &&
              SetEnvironmentVariableW(L"BMUX_WIN_TEST_STATION",
                                      station.name.c_str()) &&
              SetEnvironmentVariableW(L"BMUX_CLIPBOARD_ISOLATED_TEST", L"1"),
          "child environment");
      printf("private station=%ls desktop=BmuxDesk\n", station.name.c_str());
      matrix(station, self, argv[2], argv[3]);
      return 0;
    }
    require(argc == 4,
            "usage: lane.exe run ABSOLUTE_SHIPPING_EXE ABSOLUTE_CANDIDATE_EXE");
    peer(argv[1], argv[2], std::stoul(argv[3]));
    return 0;
  } catch (const std::exception &e) {
    fprintf(stderr, "lane: %s\n", e.what());
    return 1;
  }
}
