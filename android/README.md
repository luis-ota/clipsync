# ClipSync Android

Cliente Android v0.1 para o daemon ClipSync. Descobre `_clipsync._tcp.` por mDNS, pareia por PIN e sincroniza texto e imagens pela LAN usando WebSocket.

## Requisitos e build

- Android SDK Platform 35 e Build Tools 35.0.0
- JDK 17
- Dispositivo Android 10 (API 29) ou superior

Na raiz de `android/`:

```sh
./gradlew assembleDebug
./gradlew lint
./gradlew test
```

O APK e gerado em `app/build/outputs/apk/debug/app-debug.apk`. Para instalar com um dispositivo conectado, use `./gradlew installDebug`.

## Uso

1. Inicie `clipsyncd` no computador e mantenha ambos os dispositivos na mesma LAN confiavel.
2. Abra o app. Ele inicia o foreground service e procura os anuncios mDNS automaticamente.
3. Toque em um servidor se ele nao tiver sido selecionado automaticamente.
4. No primeiro acesso, leia o PIN de 6 digitos exibido pelo daemon e informe-o no app.
5. O `device_id` recebido em `pair_ok` fica em `SharedPreferences`; reconexoes futuras dispensam o PIN enquanto o computador confiar no aparelho.

O protocolo v0.1 usa `ws://` sem TLS. Nao use em Wi-Fi publico ou outra rede nao confiavel.

## Arquitetura

- `data/Protocol.kt`: modelos `kotlinx.serialization` compativeis com o protocolo v1.
- `data/Pairing.kt`: maquina de handshake/pairing e persistencia do `device_id`.
- `data/NsdDiscovery.kt`: browse mDNS com `NsdManager` e multicast lock.
- `net/WebSocketClient.kt`: OkHttp WebSocket, keepalive e backoff exponencial de 1 a 60 segundos.
- `service/ClipboardSyncService.kt`: foreground service que coordena discovery, sessao e clipboard.
- `service/ClipboardWatcher.kt`: texto/imagem, limite de 25 MiB, validacao e anti-eco SHA-256.
- `MainActivity.kt`: UI Compose para estado, servidores e PIN.

## Limitacoes do Android

Desde o Android 10, somente o app em foco ou o teclado padrao (IME) pode ler o clipboard. Um foreground service e sua notificacao nao ignoram essa restricao. Assim, copiar no Android e enviar ao computador e confiavel enquanto a tela do ClipSync esta em primeiro plano; em segundo plano o sistema pode ocultar o conteudo e nao entregar uma leitura utilizavel. O app continua conectado e pode receber conteudo remoto, escrevendo-o no clipboard.

No Android 13+, o usuario pode negar `POST_NOTIFICATIONS`. O foreground service ainda aparece no gerenciador de tarefas do sistema, mas a notificacao pode nao aparecer na gaveta. No Android 15, foreground services do tipo `dataSync` estao sujeitos ao limite agregado de execucao imposto pelo sistema quando o app permanece em segundo plano; reabrir o app permite reiniciar uma sessao elegivel.

Imagens recebidas sao armazenadas no cache privado e expostas ao clipboard por `FileProvider`. Imagens acima de 25 MiB, MIME nao reconhecido, base64 invalido ou SHA-256 divergente sao recusadas. HTML recebido usa o texto alternativo (ou o proprio HTML como fallback); envio HTML nao faz parte do escopo v0.1.
