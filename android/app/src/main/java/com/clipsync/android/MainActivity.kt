package com.clipsync.android

import android.Manifest
import android.content.Intent
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.clipsync.android.data.AppRepository
import com.clipsync.android.data.AppUiState
import com.clipsync.android.data.ConnectionStatus
import com.clipsync.android.data.DiscoveredServer
import com.clipsync.android.data.DeviceStore
import com.clipsync.android.data.PairingDeepLinks
import java.net.URI
import com.clipsync.android.service.ClipboardSyncService
import com.clipsync.android.net.isValidTlsFingerprint

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        RemoteEndpointStoreHolder.initialize(this)
        handlePairingIntent(intent)
        ContextCompat.startForegroundService(this, Intent(this, ClipboardSyncService::class.java))
        setContent { ClipSyncApp() }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        handlePairingIntent(intent)
    }

    private fun handlePairingIntent(intent: Intent?) {
        val link = intent?.data?.let(PairingDeepLinks::parse) ?: return
        link.serverId?.let(AppRepository::select)
        link.pin?.let(AppRepository::submitPin)
    }
}

@Composable
private fun ClipSyncApp() {
    val state by AppRepository.state.collectAsStateWithLifecycle()
    val notificationPermission = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { }
    LaunchedEffect(Unit) {
        if (Build.VERSION.SDK_INT >= 33) notificationPermission.launch(Manifest.permission.POST_NOTIFICATIONS)
    }
    MaterialTheme(colorScheme = MaterialTheme.colorScheme.copy(
        primary = Color(0xFFB43B22), secondary = Color(0xFF1E6654),
        background = Color(0xFFFFF8F0), surface = Color(0xFFFFFBF7),
    )) {
        Surface(modifier = Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
            Column(modifier = Modifier.padding(horizontal = 20.dp, vertical = 28.dp)) {
                Text("CLIPSYNC", style = MaterialTheme.typography.labelLarge, color = MaterialTheme.colorScheme.primary)
                Text("Clipboard na sua rede", style = MaterialTheme.typography.headlineMedium, fontWeight = FontWeight.Bold)
                Spacer(Modifier.height(20.dp))
                StatusCard(state)
                state.lastRemoteItem?.let { item ->
                    Spacer(Modifier.height(8.dp))
                    Text("Ultimo item do PC: ${item.preview}", style = MaterialTheme.typography.bodySmall)
                }
                if (state.status == ConnectionStatus.WAITING_FOR_PIN) PinForm()
                RemoteEndpointForm()
                Spacer(Modifier.height(24.dp))
                Text("Computadores encontrados", style = MaterialTheme.typography.titleMedium, fontWeight = FontWeight.SemiBold)
                Spacer(Modifier.height(8.dp))
                if (state.servers.isEmpty()) {
                    Text("Nenhum servidor mDNS encontrado.", color = MaterialTheme.colorScheme.onSurfaceVariant)
                } else {
                    LazyColumn(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        items(state.servers, key = DiscoveredServer::id) { server ->
                            ServerRow(server, state.selectedServerId == server.id)
                        }
                    }
                }
                Spacer(Modifier.weight(1f))
                Text(
                    "Android 10+ permite ler o clipboard somente com o app em primeiro plano. A notificacao mantem a conexao e o recebimento remoto, mas nao contorna essa regra.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
    }
}

@Composable
private fun RemoteEndpointForm() {
    var name by remember { mutableStateOf("") }
    var url by remember { mutableStateOf("") }
    var fingerprint by remember { mutableStateOf("") }
    var token by remember { mutableStateOf("") }
    var keyMaterial by remember { mutableStateOf("") }
    var deviceId by remember { mutableStateOf("") }
    Column {
        Text("Adicionar relay ou endpoint remoto", style = MaterialTheme.typography.titleMedium, fontWeight = FontWeight.SemiBold)
        OutlinedTextField(name, { name = it }, Modifier.fillMaxWidth(), label = { Text("Nome") }, singleLine = true)
        OutlinedTextField(url, { url = it }, Modifier.fillMaxWidth(), label = { Text("URL wss://host:porta/ws") }, singleLine = true)
        OutlinedTextField(fingerprint, { fingerprint = it.filter { char -> char in "0123456789abcdefABCDEF" }.lowercase().take(64) }, Modifier.fillMaxWidth(), label = { Text("Fingerprint SHA-256 (64 hex)") }, singleLine = true)
        OutlinedTextField(deviceId, { deviceId = it.trim() }, Modifier.fillMaxWidth(), label = { Text("device_id associado ao bearer") }, singleLine = true)
        OutlinedTextField(token, { token = it }, Modifier.fillMaxWidth(), label = { Text("Bearer token do relay") }, singleLine = true, visualTransformation = PasswordVisualTransformation(), keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password))
        OutlinedTextField(keyMaterial, { keyMaterial = it }, Modifier.fillMaxWidth(), label = { Text("Chave E2E: key_id group_id hex_key") }, singleLine = true, visualTransformation = PasswordVisualTransformation())
        Button(onClick = {
            val parsed = runCatching { URI(url) }.getOrNull()
            if (parsed?.scheme == "wss" && parsed.path == "/ws" && parsed.host != null && parsed.port > 0 && isValidTlsFingerprint(fingerprint) && deviceId.isNotBlank() && keyMaterial.trim().split(Regex("\\s+")).size == 3) {
                val reference = "remote:$name"
                val endpoint = DiscoveredServer(reference, null, name, parsed.host, parsed.port, true, fingerprint, reference, true, deviceId, reference)
                AppRepository.addRemoteEndpoint(endpoint)
                RemoteEndpointStoreHolder.save(endpoint)
                RemoteEndpointStoreHolder.saveToken(reference, token)
                RemoteEndpointStoreHolder.saveKey(reference, keyMaterial)
                name = ""; url = ""; fingerprint = ""; deviceId = ""; token = ""; keyMaterial = ""
            }
        }, enabled = name.isNotBlank() && url.isNotBlank() && isValidTlsFingerprint(fingerprint) && deviceId.isNotBlank() && token.isNotBlank() && keyMaterial.trim().split(Regex("\\s+")).size == 3, modifier = Modifier.fillMaxWidth()) { Text("Salvar endpoint") }
    }
}

private object RemoteEndpointStoreHolder {
    private var store: DeviceStore? = null
    fun initialize(context: android.content.Context) { store = DeviceStore(context) }
    fun save(endpoint: DiscoveredServer) { store?.saveEndpoints(AppRepository.state.value.servers.filter(DiscoveredServer::remote) + endpoint) }
    fun saveToken(reference: String, token: String) { store?.saveRelayToken(reference, token) }
    fun saveKey(reference: String, key: String) { store?.saveRelayKey(reference, key) }
}

@Composable
private fun StatusCard(state: AppUiState) {
    Card(colors = CardDefaults.cardColors(containerColor = Color(0xFFF1E7DC))) {
        Row(Modifier.fillMaxWidth().padding(16.dp), verticalAlignment = Alignment.CenterVertically) {
            Box(Modifier.width(10.dp).height(10.dp).background(
                if (state.status == ConnectionStatus.CONNECTED) Color(0xFF1E7A58) else Color(0xFFC56A27),
                RoundedCornerShape(5.dp),
            ))
            Spacer(Modifier.width(12.dp))
            Column {
                Text(state.status.name.replace('_', ' '), fontWeight = FontWeight.Bold)
                Text(state.statusDetail, style = MaterialTheme.typography.bodyMedium)
            }
        }
    }
}

@Composable
private fun ServerRow(server: DiscoveredServer, selected: Boolean) {
    Card(
        modifier = Modifier.fillMaxWidth().clickable { AppRepository.select(server.id) },
        colors = CardDefaults.cardColors(containerColor = if (selected) Color(0xFFDDECE6) else MaterialTheme.colorScheme.surface),
    ) {
        Row(Modifier.fillMaxWidth().padding(14.dp), horizontalArrangement = Arrangement.SpaceBetween) {
            Column {
                Text(server.name, fontWeight = FontWeight.SemiBold)
                Text("${server.host}:${server.port}", style = MaterialTheme.typography.bodySmall)
            }
            Text(if (selected) "SELECIONADO" else "CONECTAR", style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.secondary)
        }
    }
}

@Composable
private fun PinForm() {
    var pin by remember { mutableStateOf("") }
    Spacer(Modifier.height(16.dp))
    OutlinedTextField(
        value = pin,
        onValueChange = { pin = it.filter(Char::isDigit).take(6) },
        modifier = Modifier.fillMaxWidth(),
        label = { Text("PIN de 6 digitos") },
        singleLine = true,
        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.NumberPassword),
    )
    Spacer(Modifier.height(8.dp))
    Button(onClick = { AppRepository.submitPin(pin) }, enabled = pin.length == 6, modifier = Modifier.fillMaxWidth()) {
        Text("Parear")
    }
}
