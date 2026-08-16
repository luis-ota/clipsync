# App Android — guia de implementação

Este documento descreve como implementar o client Android para o
`clipsync` v0.1. O protocolo completo está em [PROTOCOL.md](PROTOCOL.md).

## Stack sugerida

- **Kotlin** + Jetpack Compose (minSdk 29, targetSdk 35).
- **WebSocket**: `okhttp` (inclui client WS) ou `ktor-client`.
- **Clipboard**: `ClipboardManager` do Android (API nativa, sem
  permissão especial para texto; imagem usa `ClipData.Item` com
  `contentUri`).
- **Descoberta na LAN**: `NsdManager` (browse de `_clipsync._tcp.local.`)
  ou biblioteca `mdns` (ex: `com.github.jvanderzandt:mDNSResponder`).
- **Armazenamento do device_id**: `SharedPreferences` (persistência
  do `device_id` recebido em `pair_ok`).

## Estrutura

```
app/src/main/java/com/clipsync/android/
├── MainActivity.kt        UI (lista de peers, estado de conexão)
├── data/
│   ├── Pairing.kt         device_id persistido, nonce
│   ├── NsdDiscovery.kt    browse mDNS → lista de servidores
│   └── Protocol.kt        tipos JSON (Hello, PairChallenge, …)
├── net/
│   ├── WebSocketClient.kt conexão + keepalive + reconnect
│   └── MessageSerializer.kt (kotlinx.serialization)
└── service/
    ├── ClipboardSyncService.kt (foreground service)
    └── ClipboardWatcher.kt   ContentObserver no clipboard
```

## Fluxo de vida do app

1. **Startup**: inicia `ClipboardSyncService` (foreground com
   notification persistente).
2. **Descoberta**: browse mDNS `_clipsync._tcp.local.` → lista de
   daemons (nome, IP, porta).
3. **Conexão**: `ws://<ip>:<porta>/ws`, envia `hello` com
   `device.name`, `device.kind = "android"` e `device.id` (salvo).
4. **Pareamento** (se necessário):
   - Recebe `pair_challenge` → mostra o PIN na UI (6 dígitos).
   - Usuário digita → envia `pair_submit` com `code` + `nonce`.
   - Recebe `pair_ok` → salva `device_id`, `server_name`.
5. **Sync**:
   - Clipboard local mudou → envia `clipboard_text` (ou `clipboard_image`).
   - Recebe mensagem remota → escreve no clipboard local.
   - Envia `pong` em resposta a `ping`.
6. **Reconnect**: com backoff exponencial (1s, 2s, 4s… max 60s) e
   re-browse mDNS quando perde a rede (WifiManager / ConnectivityManager).

## Handshake — detalhes

```kotlin
data class Hello(
    val type: String = "hello",
    val v: Int = 1,
    val device: DeviceInfo
)
data class DeviceInfo(
    val name: String,
    val kind: String,          // "android"
    val id: String? = null,    // salvo após pair_ok
    val os_version: String? = Build.VERSION.RELEASE,
    val app_version: String? = "clipsync-android 0.1.0"
)
```

> O `device_id` **deve** ser persistido. Sem ele, o servidor vai
> pedir pareamento toda vez que reconectar.

## Clipboard local

- **Enviar**: registrar `ClipboardManager.OnPrimaryClipChangedListener`
  e ler `clipboard.primaryClip`. Para texto: `clipData.getItemAt(0).text`.
  Para imagem: `item.uri` → `contentResolver` → bytes → base64.
- **Receber**: gravar `ClipData.newPlainText(...)` ou `newUri(...)`.
- **Dica**: manter um `lastSentSha256` local para evitar eco, igual ao
  daemon (`last_self_write`). Compare o sha256 do conteúdo antes de
  enviar.

## Keepalive

- Ao receber `ping`, responder `pong` imediatamente.
- Se não houver mensagem do servidor em 45s, assumir conexão morta e
  tentar reconectar.

## Permissões (AndroidManifest.xml)

```xml
<uses-permission android:name="android.permission.INTERNET" />
<uses-permission android:name="android.permission.FOREGROUND_SERVICE" />
<uses-permission android:name="android.permission.ACCESS_WIFI_STATE" />
<uses-permission android:name="android.permission.CHANGE_WIFI_MULTICAST_STATE" />
<uses-permission android:name="android.permission.WAKE_LOCK" />
```

`CHANGE_WIFI_MULTICAST_STATE` é necessária para receber pacotes mDNS
em redes Wi-Fi (multicast bloqueado por padrão no Android).

## Segurança

- v0.1: sem TLS. Para uso fora de rede confiável, espere a v0.2
  (TLS + AES-GCM) ou use uma VLAN/rota criptografada.
- O PIN exibido pelo daemon (`clipsyncd show-pin`) tem TTL curto
  (default 120s) — force o usuário a digitar rápido.

## Testes (esboço)

- `ProtocolSerializerTest`: round-trip de todas as mensagens JSON.
- `HandshakeTest`: mock de servidor WebSocket local → hello/pair_ok.
- `ReconnectTest`: server derruba conexão → backoff → reconnect.
