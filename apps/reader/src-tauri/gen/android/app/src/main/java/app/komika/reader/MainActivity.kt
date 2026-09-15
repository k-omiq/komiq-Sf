package app.komika.reader

import android.os.Bundle
import android.view.View
import androidx.activity.enableEdgeToEdge
import androidx.core.graphics.Insets
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    insetContentForOldWebView()
  }

  // The page draws under the status and navigation bars and steps its own bars in with
  // CSS env(safe-area-inset-*); the keyboard shrinks its visual viewport. The WebView
  // does both by itself from version 139 (system bars from 136, keyboard from 139).
  // An older one reports the display cutout only, so the bottom tab bar lands under the
  // navigation buttons and the keyboard covers the field being typed in. For those,
  // keep the WebView out from under the bars and the keyboard, and pass it zero insets
  // so nothing is counted twice. The uncovered strips show the window background
  // (@color/app_background).
  private fun insetContentForOldWebView() {
    val webViewMajor = version.substringBefore('.').toIntOrNull() ?: return
    if (webViewMajor >= 139) return
    val types = WindowInsetsCompat.Type.systemBars() or
      WindowInsetsCompat.Type.displayCutout() or
      WindowInsetsCompat.Type.ime()
    ViewCompat.setOnApplyWindowInsetsListener(findViewById<View>(android.R.id.content)) { view, insets ->
      val covered = insets.getInsets(types)
      view.setPadding(covered.left, covered.top, covered.right, covered.bottom)
      WindowInsetsCompat.Builder(insets).setInsets(types, Insets.NONE).build()
    }
  }
}
