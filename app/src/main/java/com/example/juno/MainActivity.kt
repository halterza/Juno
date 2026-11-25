package com.example.chocolatesugar

import android.content.Context
import android.content.SharedPreferences
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.systemBarsPadding
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext
import org.json.JSONArray
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest

private val ChocolateDark = Color(0xFF1F110B)
private val ChocolateMid = Color(0xFF3E2418)
private val ChocolateLight = Color(0xFF5A3A29)
private val White = Color.White
private val WhiteSoft = Color.White.copy(alpha = 0.6f)
private val WhiteFaint = Color.White.copy(alpha = 0.35f)

private val Minimal = FontFamily.SansSerif

data class Reading(val mmol: Double, val arrow: String, val minutesAgo: Long)

private fun sha1(s: String): String =
    MessageDigest.getInstance("SHA-1")
        .digest(s.toByteArray())
        .joinToString("") { "%02x".format(it) }

private fun arrowFor(direction: String?): String = when (direction) {
    "DoubleUp" -> "⇈"
    "SingleUp" -> "↑"
    "FortyFiveUp" -> "↗"
    "Flat" -> "→"
    "FortyFiveDown" -> "↘"
    "SingleDown" -> "↓"
    "DoubleDown" -> "⇊"
    else -> ""
}

private suspend fun fetchReading(baseUrl: String, secret: String): Reading =
    withContext(Dispatchers.IO) {
        val base = baseUrl.trim().trimEnd('/').let {
            if (it.startsWith("http://") || it.startsWith("https://")) it else "https://$it"
        }
        val conn = URL("$base/api/v1/entries/sgv.json?count=1").openConnection() as HttpURLConnection
        try {
            conn.setRequestProperty("api-secret", sha1(secret.trim()))
            conn.setRequestProperty("Accept", "application/json")
            conn.connectTimeout = 10_000
            conn.readTimeout = 10_000
            if (conn.responseCode != 200) error("Error: ${conn.responseCode}")

            val arr = JSONArray(conn.inputStream.bufferedReader().readText())
            if (arr.length() == 0) error("Нет данных")

            val o = arr.getJSONObject(0)
            val mgdl = o.getDouble("sgv")
            val dateMs = o.getLong("date")
            Reading(
                mmol = mgdl / 18.0,
                arrow = arrowFor(o.optString("direction")),
                minutesAgo = ((System.currentTimeMillis() - dateMs) / 60_000).coerceAtLeast(0)
            )
        } finally {
            conn.disconnect()
        }
    }

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val prefs = getSharedPreferences("nightscout", Context.MODE_PRIVATE)
        setContent { ChocolateApp(prefs) }
    }
}

@Composable
fun ChocolateApp(prefs: SharedPreferences) {
    var url by remember { mutableStateOf(prefs.getString("url", "") ?: "") }
    var secret by remember { mutableStateOf(prefs.getString("secret", "") ?: "") }
    var loggedIn by remember { mutableStateOf(url.isNotBlank() && secret.isNotBlank()) }

    Box(
        Modifier
            .fillMaxSize()
            .background(Brush.verticalGradient(listOf(ChocolateLight, ChocolateMid, ChocolateDark)))
            .systemBarsPadding()
    ) {
        if (loggedIn) {
            GlucoseScreen(
                url = url,
                secret = secret,
                onLogout = {
                    prefs.edit().clear().apply()
                    secret = ""
                    loggedIn = false
                }
            )
        } else {
            LoginScreen(
                url = url,
                secret = secret,
                onUrl = { url = it },
                onSecret = { secret = it },
                onConnect = {
                    prefs.edit().putString("url", url).putString("secret", secret).apply()
                    loggedIn = true
                }
            )
        }
    }
}

@Composable
private fun LoginScreen(
    url: String,
    secret: String,
    onUrl: (String) -> Unit,
    onSecret: (String) -> Unit,
    onConnect: () -> Unit
) {
    val fieldColors = OutlinedTextFieldDefaults.colors(
        focusedTextColor = White,
        unfocusedTextColor = White,
        cursorColor = White,
        focusedBorderColor = White,
        unfocusedBorderColor = WhiteFaint,
        focusedLabelColor = White,
        unfocusedLabelColor = WhiteSoft,
    )
    val fieldText = TextStyle(fontFamily = Minimal, fontWeight = FontWeight.Light, fontSize = 16.sp)

    Column(
        Modifier
            .fillMaxSize()
            .padding(32.dp),
        verticalArrangement = Arrangement.Center
    ) {
        Text(
            "nightscout",
            color = White,
            fontFamily = Minimal,
            fontWeight = FontWeight.ExtraLight,
            fontSize = 36.sp,
            letterSpacing = 4.sp
        )
        Spacer(24)

        OutlinedTextField(
            value = url,
            onValueChange = onUrl,
            label = { Text("URL", fontFamily = Minimal, fontWeight = FontWeight.Light) },
            placeholder = { Text("my-site.example.com", color = WhiteFaint) },
            singleLine = true,
            textStyle = fieldText,
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
            colors = fieldColors,
            modifier = Modifier.fillMaxWidth()
        )
        Spacer(16)
        OutlinedTextField(
            value = secret,
            onValueChange = onSecret,
            label = { Text("API secret", fontFamily = Minimal, fontWeight = FontWeight.Light) },
            singleLine = true,
            textStyle = fieldText,
            visualTransformation = PasswordVisualTransformation(),
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password),
            colors = fieldColors,
            modifier = Modifier.fillMaxWidth()
        )
        Spacer(32)

        Button(
            onClick = onConnect,
            enabled = url.isNotBlank() && secret.isNotBlank(),
            colors = ButtonDefaults.buttonColors(
                containerColor = White,
                contentColor = ChocolateDark,
                disabledContainerColor = WhiteFaint,
                disabledContentColor = ChocolateMid
            ),
            modifier = Modifier.fillMaxWidth().height(52.dp)
        ) {
            Text("connect", fontFamily = Minimal, fontWeight = FontWeight.Normal, fontSize = 16.sp)
        }
    }
}

@Composable
private fun GlucoseScreen(url: String, secret: String, onLogout: () -> Unit) {
    var reading by remember { mutableStateOf<Reading?>(null) }
    var error by remember { mutableStateOf<String?>(null) }

    LaunchedEffect(url, secret) {
        while (true) {
            try {
                reading = fetchReading(url, secret)
                error = null
            } catch (e: Exception) {
                error = e.message ?: "cannot take the data"
            }
            delay(60_000)
        }
    }

    Column(
        Modifier.fillMaxSize(),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center
    ) {
        val r = reading
        if (r != null) {
            Row(verticalAlignment = Alignment.Bottom) {
                Text(
                    "%.1f".format(r.mmol),
                    color = White,
                    fontFamily = Minimal,
                    fontWeight = FontWeight.Thin,
                    fontSize = 120.sp
                )
                Text(
                    " ${r.arrow}",
                    color = WhiteSoft,
                    fontFamily = Minimal,
                    fontWeight = FontWeight.ExtraLight,
                    fontSize = 48.sp,
                    modifier = Modifier.padding(bottom = 22.dp)
                )
            }
            Text(
                "mmol/l",
                color = WhiteSoft,
                fontFamily = Minimal,
                fontWeight = FontWeight.Light,
                fontSize = 18.sp,
                letterSpacing = 3.sp
            )
            Spacer(16)
            Text(
                if (r.minutesAgo == 0L) "right now" else "${r.minutesAgo} minutes ago",
                color = WhiteFaint,
                fontFamily = Minimal,
                fontWeight = FontWeight.Light,
                fontSize = 14.sp
            )
        } else if (error == null) {
            Text(
                "…",
                color = WhiteSoft,
                fontFamily = Minimal,
                fontWeight = FontWeight.Thin,
                fontSize = 64.sp
            )
        }

        error?.let {
            Spacer(24)
            Text(
                it,
                color = WhiteSoft,
                fontFamily = Minimal,
                fontWeight = FontWeight.Light,
                fontSize = 14.sp,
                textAlign = TextAlign.Center,
                modifier = Modifier.padding(horizontal = 32.dp)
            )
        }

        Spacer(48)
        TextButton(onClick = onLogout) {
            Text(
                "leave",
                color = WhiteFaint,
                fontFamily = Minimal,
                fontWeight = FontWeight.Light,
                fontSize = 13.sp,
                letterSpacing = 2.sp
            )
        }
    }
}

@Composable
private fun Spacer(h: Int) = androidx.compose.foundation.layout.Spacer(Modifier.height(h.dp))
