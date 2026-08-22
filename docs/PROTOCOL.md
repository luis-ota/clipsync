# Protocolo clipsync v1

> Protocolo de aplicação trocado entre o daemon (`clipsyncd`) e os
> clients (Android e clientes desktop). Transporte padrão: **WebSocket sobre
> TLS** (RFC 6455 + RFC 8446).

Todas as mensagens são **JSON** em frames de texto WebSocket, com um
discriminador `type` e a versão do protocolo `v`.

Versão atual: **1**. Mudanças incompatíveis incrementam `v` e exigem
um novo handshake.

## Tabela de mensagens

| Tipo               | Direção          | Descrição                        |
|--------------------|------------------|----------------------------------|
| `hello`            | client → server  | Identifica o device              |
| `pair_challenge`   | server → client  | Desafio (nonce + challenge_id); o PIN nunca viaja |
| `pair_submit`      | client → server  | Submete PIN digitado + nonce + challenge_id |
| `pair_ok`          | server → client  | Pareamento confirmado, device_id |
| `pair_fail`        | server → client  | PIN inválido/expirado            |
| `clipboard_text`   | ↔                | Sincroniza texto plain           |
| `clipboard_image`  | ↔                | Sincroniza imagem (base64)       |
| `clipboard_html`   | ↔                | Sincroniza rich text (v0.2)      |
| `ping` / `pong`    | ↔                | Keepalive                        |
| `error`            | server → client  | Erro genérico                    |

## Fluxo de conexão

```
CLIENT                            SERVER
  │  wss://192.168.1.50:8765/ws     │
  │────────────────────────────────>│
  │  {"type":"hello","v":1,         │
  │   "device":{"name":"Pixel 8",   │
  │             "kind":"android",   │
  │             "id":null}}         │
  │────────────────────────────────>│
  │                                 │
  │  ── se id é confiado ──>        │
  │        {"type":"pair_ok",       │
  │         "device_id":"…",        │
  │         "server_id":"…",        │
  │         "session_id":"…",       │
  │         "server_name":"luis"}   │
  │<────────────────────────────────│
  │                                 │
  │  ── se id é novo ──>            │
  │        {"type":"pair_challenge",│
  │         "challenge_id":"uuid",  │
  │         "expires_at":1723…,     │
  │         "nonce":"a1b2…"}        │
  │<────────────────────────────────│
  │                                 │
  │  (usuário lê o PIN exibido no   │
  │   daemon e digita no app)       │
  │                                 │
  │  {"type":"pair_submit",         │
  │   "challenge_id":"uuid",        │
  │   "code":"834921",              │
  │   "nonce":"a1b2…"}              │
  │────────────────────────────────>│
  │                                 │
  │        {"type":"pair_ok",       │
  │         "device_id":"uuid",     │
  │         "server_id":"uuid",     │
  │         "session_id":"uuid",    │
  │         "server_name":"luis"}   │
  │<────────────────────────────────│
```

> O PIN de 6 dígitos é gerado pelo servidor e **exibido localmente no
> daemon** (bandeja/tray ou log do processo em foreground). A resposta de
> `pair_challenge` carrega apenas `challenge_id`, `expires_at` e `nonce`
> — **o PIN nunca atravessa o fio**.

### Regras

1. A **primeira** mensagem de uma conexão **deve** ser `hello`.
   O servidor fecha a conexão se receber outra coisa primeiro.
2. Se `device.id` está na lista de confiados do servidor, o pareamento
   é pulado e `pair_ok` é enviado imediatamente.
3. Se `device.id` é `null` (ou desconhecido), o servidor gera um PIN de
   6 dígitos, o **exibe localmente no daemon** (bandeja/tray ou log) e envia
   `pair_challenge` com `challenge_id`,
   `nonce` e `expires_at`. O client **não** recebe o PIN — ele é
   digitado pelo usuário a partir da exibição no daemon. O desafio fica
   associado à conexão que recebeu o `pair_challenge`; o nome anunciado no
   `hello` não é uma chave de autenticação.
4. Existe no máximo um desafio ativo no daemon. Um novo `hello` de device
   desconhecido substitui o desafio anterior para que o único PIN exibido no
   tray seja sempre o único PIN aceito.
5. `pair_submit` com PIN correto (digitado) + `challenge_id` + nonce
   corretos → `pair_ok`. PIN errado → `pair_fail` e o servidor fecha a
   conexão.
5. O `device_id` recebido em `pair_ok` **deve** ser persistido pelo
   client sob a chave `server_id` e enviado em `hello` somente nas próximas
   conexões com esse servidor. `server_id` é a identidade persistida do daemon,
   também anunciada por mDNS; host, porta e nome nunca são chaves de confiança.
   Clients antigos podem ignorar o novo campo sem quebrar a decodificação.
6. Se um `device_id` já tem uma sessão ativa e reconecta, a **nova**
   sessão substitui a antiga. A sessão antiga recebe `error` com
   código `superseded` e para de receber broadcasts; o client deve
   fechar a conexão ao recebê-lo.

## Mensagens de clipboard

> O campo `origin` é **autoritativo no servidor**: ao repassar uma
> mensagem de clipboard, o servidor sobrepõe `origin` com o
> `device_id` autenticado da sessão remetente, ignorando o valor
> declarado pelo client. Clients não devem forjar `origin` — o valor
> final é sempre o do remetente autenticado. O daemon emite `origin`
> com seu próprio `device_id` persistido (estável por sessão), nunca
> um UUID novo por frame, para que o dedup `last_origin + last_seq`
> dos clients funcione.

### Texto

```json
{
  "type": "clipboard_text",
  "mime": "text/plain;charset=utf-8",
  "content": "https://exemplo.com",
  "origin": "uuid-do-device",
  "sha256": "hex-64-chars"
}
```

### Imagem

Imagens são enviadas **inline em base64** (standard, sem line breaks)
no campo `data_b64`:

```json
{
  "type": "clipboard_image",
  "mime": "image/png",
  "data_b64": "iVBORw0KGgoAAAANSUhEUgAA…",
  "width": 1080,
  "height": 1920,
  "sha256": "hex-64-chars",
  "origin": "uuid-do-device"
}
```

O daemon aceita até 25 MiB (configurável em `max_image_bytes`). Clients que
usam OkHttp e JSON/base64 devem aplicar o menor limite end-to-end: o Android
reserva 8 KiB para o envelope e limita os bytes crus a 12 MiB menos essa
reserva codificada, mantendo cada mensagem abaixo da fila de 16 MiB do OkHttp.
No servidor, `max_message_size` e `max_frame_size` do WebSocket são calculados
a partir de `max_image_bytes` e `max_text_bytes`, incluindo a inflação do
base64, o escaping JSON e 8 KiB de envelope. Os limites específicos de cada
payload continuam sendo validados depois do parsing.
### Transferência binária

O caminho v1 legado não muda: texto e imagens pequenas continuam em JSON, e
clientes antigos podem ignorar os novos tipos. Conteúdo grande usa uma oferta
JSON e, somente depois de `transfer_accept`, frames WebSocket binários:

```json
{"type":"transfer_offer","transfer_id":"uuid","mime":"application/octet-stream","name":"arquivo.zip","size":1048576,"chunks":16,"sha256":"hex-64-chars","file":true,"origin":"uuid-do-device"}
{"type":"transfer_accept","transfer_id":"uuid"}
```

Arquivos (`file=true`) exigem confirmação explícita no Android antes de aceitar;
o padrão é rejeitar. Cada frame tem envelope `CSB1` de 38 bytes, seguido de no
máximo 64 KiB de dados. O total é limitado a 256 MiB, chunks são ordenados e o
receptor grava incrementalmente em arquivo temporário, com memória limitada a
um chunk. Só publica após conferir tamanho e SHA-256; tamper, repetição ou salto
de índice abortam a transferência. O remetente aplica backpressure e mantém no
máximo quatro chunks em voo.

No backend Linux X11, somente `image/png` e `image/jpeg` são aceitos. O daemon
consulta os alvos anunciados pelo clipboard antes de ler e usa `xclip` com o
MIME explícito ao escrever. `image/gif` e outros MIME de imagem não são
sincronizados. O limite também é aplicado antes do broadcast local, além da
validação das mensagens recebidas.
O sender deduplica clipboard por `sha256` dentro de cada sessão de peer:
uma cópia repetida não gera outro envio. Não há `ack` ou referência nova no
protocolo; ao reconectar, o peer recebe novamente o primeiro conteúdo visto.
O debounce do watcher é trailing e de 300 ms, portanto uma sequência local
rápida envia apenas o último snapshot.

Compressão e headers compartilhados não foram adicionados: o transporte atual
não negocia `permessage-deflate`, e remover campos do handshake seria uma
mudança de protocolo sem ganho medido. `cargo bench --bench perf` registra o
tamanho JSON e o tempo de serialização para texto de 1 KiB e imagem de 1 MiB.

### HTML (rich text, v0.2)

```json
{
  "type": "clipboard_html",
  "html": "<b>negrito</b>",
  "alt": "negrito",
  "origin": "uuid-do-device",
  "sha256": "hex-64-chars"
}
```

| Campo     | Tipo   | Descrição                                              |
|-----------|--------|--------------------------------------------------------|
| `html`    | string | Conteúdo rich text (text/html).                        |
| `alt`     | string \| null | Texto plain alternativo (fallback para peers/clients que não suportam `text/html`). Opcional; `null` quando só há HTML disponível. |
| `origin`  | string | Device que originou o conteúdo (anti-eco).             |
| `sha256`  | string | SHA-256 (hex) do conteúdo de `html` (dedup + anti-eco).|

> O daemon lê `text/html` via `wl-paste --type text/html` (Wayland) e
> grava via `wl-copy -t text/html`. Em X11/headless o HTML não é lido
> (apenas texto plain). Habilitado pela config `clipboard.sync_html`
> (default `false`).

## Keepalive

O servidor envia `{"type":"ping"}` a cada 30s e espera `pong` do
client. Client que não responder em 60s é considerado morto e
desconectado.

```json
{"type":"ping","ts":1234567890}
{"type":"pong","ts":1234567890}
```

## Segurança (v1)

### Contrato de relay (#75/#76)

O núcleo expõe `clipsync_core::auth` para relays e clientes: `ServerId`,
`UserId`, `GroupId`, `SessionId` e `Principal` são tipos distintos e
serializáveis; uma `SessionCredential` contém um token aleatório de 256 bits
emitido após o pareamento pela API de autenticação. O token é bearer e só é
válido sobre o transporte TLS. Relays devem autenticar a sessão antes de
aceitar clipboard.

O `RelayEnvelope` exige `source` igual ao device autenticado, uma sequência
estritamente crescente por `SessionId`, e autorização de `source` e
`destination` no mesmo `GroupId`. A autorização rejeita grupo desconhecido,
cross-group forwarding, destino não membro, source forjado e replay. Depois
da validação, `origin` é sobrescrito pelo source autenticado.

Frames relay de clipboard usam `type=relay_envelope` com `key_id`, `nonce` e
`ciphertext` base64. O plaintext é uma `Message` JSON, cifrada com AES-256-GCM
por uma chave de grupo provisionada aos endpoints fora do relay. O AAD vincula
`session_id`, `source`, `destination`, `group`, `sequence` e `key_id`; portanto
alterar roteamento ou sequência falha na autenticação. `sequence` deve ser
estritamente crescente por sessão e impede replay no relay. `RelayKeyRing`
mantém chaves anteriores durante rotação; remova a chave antiga após todos os
peers receberem a nova. O relay nunca possui a chave nem desserializa o
clipboard.

Compatibilidade é explícita: o caminho LAN/WebSocket direto mantém as mensagens
`clipboard_*` legadas e continua protegido pelo TLS configurado. O endpoint
relay rejeita `clipboard_*` em claro; clientes relay precisam suportar o
envelope E2E. TLS/bearer continuam necessários para autenticar e proteger o
hop, mas não são considerados E2E.

O relay envia um `PairOk` inicial com o `session_id` efêmero; nenhum envelope é
aceito antes desse handshake. O bearer é referenciado por `credential_ref` e o
material E2E separado por `e2e_key_ref`, no formato `key_id group_id hex_key`.
Nenhum segredo é colocado na URL ou registrado em logs. Rust e Android usam o
mesmo AAD canônico; chave errada, tamper, alteração de header e replay são
rejeitados.

- O transporte padrão é `wss://`. O daemon gera uma identidade autoassinada
  persistente (`tls-cert.der`/`tls-key.der`, chave com modo 0600). A confiança
  usa o fingerprint SHA-256 do certificado DER, não hostname, IP ou nome mDNS.
- O TXT mDNS publica `tls=1` e `tls_fingerprint=<64 hex>`. O Android exige os
  dois campos, usa `wss://` e rejeita certificado cujo fingerprint não coincida.
- `security.tls_fingerprint` opcional impede iniciar com uma identidade TLS
  inesperada. Para rotação, substitua os arquivos e distribua o novo pin por
  novo pareamento/registro mDNS.
- `security.transport = "plaintext_legacy"` é compatibilidade explícita com
  clients v0.1, não é o padrão, gera warning e não é aceito pelo Android atual.
- Com `security.local_only = true` (padrão), o daemon aceita WebSocket
  apenas de endereços loopback, privados ou link-local. Isso é um filtro de
  endereço, não uma prova de mesma sub-rede ou de SSID.
- `security.pairing_timeout_secs` controla a validade do desafio e o tempo
  máximo em que a conexão aguarda `pair_submit`.
- Pareamento por PIN de 6 dígitos exibido no daemon (`clipsyncd --show-pin`)
  e digitado no app. O PIN **nunca** é transmitido: `pair_challenge`
  responde apenas com `challenge_id`, `nonce` e `expires_at`; o `code`
  digitado só aparece no `pair_submit` do próprio device que está sendo
  pareado.

## Endpoints

- `wss://<host>:8765/ws` — websocket principal, com pinning mDNS.
- `https://<host>:8765/healthz` — healthcheck (200/ok), com o mesmo pin TLS.
- `https://<host>:8765/readyz` — readiness (200 enquanto aceita conexões; 503
  durante shutdown), com o mesmo pin TLS.
- `http://<host>:8765/` — info JSON do daemon (name, version).

## Detecção na LAN (mDNS)

O daemon anuncia o serviço `_clipsync._tcp.local.` na porta 8765.
O app Android deve fazer *browse* desse serviço (biblioteca `NsdManager`
do Android) para descobrir o daemon sem configuração manual.

Campos TXT:
| Chave        | Valor                       |
|--------------|-----------------------------|
| `name`       | Nome amigável do PC         |
| `server_id`  | UUID estável do daemon       |
| `protocol`   | `v1`                        |
| `port`       | Porta do websocket          |
| `host`       | IP do daemon                |
| `tls`        | `1` no transporte TLS obrigatório |
| `tls_fingerprint` | SHA-256 hex do certificado DER |
