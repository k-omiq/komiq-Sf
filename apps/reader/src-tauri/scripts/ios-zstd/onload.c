// The VM treats a library as linked into the app when the app has a function named
// JNI_OnLoad_<library name>. zstd-kmp has no JNI_OnLoad of its own, so this is it.
// The name must be the one that scripts/ios-zstd/com/squareup/zstd/Zstd__JvmZstdKt.java
// passes to System.loadLibrary.
#include <jni.h>

JNIEXPORT jint JNICALL JNI_OnLoad_zstdkmp(JavaVM *vm, void *reserved) {
  (void)vm;
  (void)reserved;
  return JNI_VERSION_1_8;
}
