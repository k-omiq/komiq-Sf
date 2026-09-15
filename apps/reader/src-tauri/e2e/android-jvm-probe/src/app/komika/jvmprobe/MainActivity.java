package app.komika.jvmprobe;

import android.app.Activity;
import android.os.Bundle;
import android.util.Log;
import java.io.File;

public class MainActivity extends Activity {
  static native int run(String libdir, String home, String jar, String data, int port);

  @Override
  protected void onCreate(Bundle b) {
    super.onCreate(b);
    System.loadLibrary("probe");
    File f = getFilesDir();
    String lib = getApplicationInfo().nativeLibraryDir;
    Log.i("jvmprobe", "nativeLibraryDir=" + lib + " files=" + f);
    int rc = run(lib, new File(f, "home").getPath(), new File(f, "zstd-android.jar").getPath() + ":" + new File(f, "Suwayomi-Server.jar").getPath(),
        new File(f, "data").getPath(), 45679);
    Log.i("jvmprobe", "run returned " + rc + " (VM thread started)");
  }
}
