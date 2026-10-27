package com.eeriegoesd.sidewire

import android.graphics.Color
import android.graphics.drawable.ColorDrawable
import android.os.Build
import android.os.Bundle
import android.view.View
import androidx.activity.SystemBarStyle
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import kotlin.math.max

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    applyBars(prefs().getBoolean(KEY_LIGHT, false))
    super.onCreate(savedInstanceState)

    // Keep the page clear of the status bar, cut-out, nav bar and keyboard.
    val content = findViewById<View>(android.R.id.content)
    ViewCompat.setOnApplyWindowInsetsListener(content) { view, insets ->
      val bars = insets.getInsets(
        WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout()
      )
      val ime = insets.getInsets(WindowInsetsCompat.Type.ime())
      view.setPadding(bars.left, bars.top, bars.right, max(bars.bottom, ime.bottom))
      WindowInsetsCompat.CONSUMED
    }
  }

  // Called from Rust through JNI (set_window_theme).
  fun setAppTheme(light: Boolean) {
    runOnUiThread {
      prefs().edit().putBoolean(KEY_LIGHT, light).apply()
      applyBars(light)
    }
  }

  private fun applyBars(light: Boolean) {
    val style = if (light) {
      SystemBarStyle.light(Color.TRANSPARENT, Color.TRANSPARENT)
    } else {
      SystemBarStyle.dark(Color.TRANSPARENT)
    }
    enableEdgeToEdge(statusBarStyle = style, navigationBarStyle = style)
    window.setBackgroundDrawable(ColorDrawable(if (light) PANEL_LIGHT else PANEL_DARK))
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
      window.isNavigationBarContrastEnforced = false
    }
  }

  private fun prefs() = getSharedPreferences("sidewire", MODE_PRIVATE)

  companion object {
    private const val KEY_LIGHT = "light_theme"
    private val PANEL_DARK = Color.parseColor("#050508")    // page background, dark
    private val PANEL_LIGHT = Color.parseColor("#f7f8fb")   // page background, light
  }
}
