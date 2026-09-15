// Stand-in for Termux's libandroid-spawn.so, which libjvm.so and libjava.so name as a
// dependency. scripts/build-android-jvm.sh compiles it into the app's library set.
//
//  1. posix_spawn: bionic has it since API 28, so there is nothing to provide; the symbol
//     resolves from libc.so. (Termux's own library is 4 KB aligned and needs
//     libc++_shared.so.)
//  2. dladdr: the Termux HotSpot takes java.home from the path of libjvm.so and from
//     nothing else (<java.home>/lib/server/libjvm.so). In an app the library is in the
//     native library dir, which is read-only and holds no `modules` file. This library
//     comes before libdl.so in libjvm.so's dependency list, so libjvm.so binds to the
//     dladdr below. It answers with the path HotSpot expects, under the java.home the app
//     gave to komiq_jvm_set_home() before it loaded libjvm.so.
#include <dlfcn.h>
#include <stdio.h>
#include <string.h>

static char jvm_path[4096];

void komiq_jvm_set_home(const char *java_home) {
  snprintf(jvm_path, sizeof jvm_path, "%s/lib/server/libjvm.so", java_home);
}

typedef int (*dladdr_fn)(const void *, Dl_info *);

int dladdr(const void *addr, Dl_info *info) {
  static dladdr_fn real;
  if (!real) {
    void *libdl = dlopen("libdl.so", RTLD_NOW);
    real = libdl ? (dladdr_fn)dlsym(libdl, "dladdr") : NULL;
    if (!real) return 0;
  }
  int rc = real(addr, info);
  if (rc && jvm_path[0] && info->dli_fname) {
    const char *slash = strrchr(info->dli_fname, '/');
    if (slash && strcmp(slash, "/libjvm.so") == 0) info->dli_fname = jvm_path;
  }
  return rc;
}
