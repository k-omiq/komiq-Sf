package com.squareup.zstd;

/**
 * The iOS copy of zstd-kmp's native-library loader. scripts/prune-ios-jar.sh puts the
 * compiled class into the iOS jar in place of the one zstd-kmp ships.
 *
 * The shipped loader reads os.name, knows "windows", "linux" and "mac", and throws
 * "Unsupported OS" for anything else. On the phone os.name is "Darwin". It also unpacks a
 * library file from the jar and loads that, which iOS does not allow.
 *
 * On iOS the library is linked into the app (libzstdkmp.a, made by
 * scripts/build-ios-zstd.sh). The VM finds a linked-in library by the function
 * JNI_OnLoad_<name>, so the name here must be the one in scripts/ios-zstd/onload.c.
 */
final class Zstd__JvmZstdKt {
  private Zstd__JvmZstdKt() {}

  public static final void loadNativeLibrary() {
    System.loadLibrary("zstdkmp");
  }
}
