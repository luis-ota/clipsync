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
- **Armazenamento do device_id**: `SharedPreferences`, indexado pelo
  `server_id` estável recebido via mDNS e confirmado em `pair_ok`.

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
2. **Descoberta**: cada rede inicia uma nova época de browse mDNS. Resultados
   anteriores são descartados; o endpoint selecionado é sempre resolvido pelo
   `server_id` no mapa da época atual.
3. **Conexão**: `wss://<ip>:<porta>/ws`, após validar `tls=1` e o
   `tls_fingerprint` do TXT mDNS, envia `hello` com
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
   re-browse mDNS quando perde a rede. Socket, retry e callbacks passam por um
   actor; todo evento leva a geração da sessão e eventos antigos são ignorados.

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

> O `device_id` **deve** ser persistido por `server_id`. A migração consome o
> antigo ID global uma única vez, no primeiro servidor estável selecionado.

## Clipboard local

- **Enviar**: registrar `ClipboardManager.OnPrimaryClipChangedListener`
  e ler `clipboard.primaryClip`. Para texto: `clipData.getItemAt(0).text`.
  Para imagem: `item.uri` → `contentResolver` → bytes → base64.
- **Receber**: gravar `ClipData.newPlainText(...)` ou `newUri(...)`.
- **Anti-eco**: manter uma fila limitada de hashes de escritas remotas com TTL.
  Cada callback consome somente sua entrada; callbacks ausentes expiram sem
  suprimir indefinidamente uma cópia legítima futura.
- Hash, base64, decode e I/O de imagem rodam fora da Main. Com OkHttp, o payload
  JSON deve ficar abaixo de 16 MiB; o app reserva 8 KiB para metadados e aceita
  no máximo `12 MiB - 6 KiB` de imagem crua.

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

- v1: `wss://` obrigatório para o cliente atual. O certificado autoassinado
  é validado pelo fingerprint `tls_fingerprint` anunciado no TXT mDNS; não há
  downgrade silencioso para `ws://`.
- O PIN exibido pelo daemon (`clipsyncd show-pin`) tem TTL curto
  (default 120s) — force o usuário a digitar rápido.

## Testes (esboço)

- `ProtocolSerializerTest`: round-trip de todas as mensagens JSON.
- `HandshakeTest`: mock de servidor WebSocket local → hello/pair_ok.
- `ReconnectTest`: server derruba conexão → backoff → reconnect.
