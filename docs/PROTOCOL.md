# Protocolo clipsync v1

> Protocolo de aplicação trocado entre o daemon (`clipsyncd`) e os
> clients (apps Android). Transporte: **WebSocket** (RFC 6455).

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
  │  ws://192.168.1.50:8765/ws     │
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
  │         "session_id":"uuid",    │
  │         "server_name":"luis"}   │
  │<────────────────────────────────│
```

> O PIN de 6 dígitos é gerado pelo servidor e **exibido localmente no
> daemon** (bandeja/tray ou `clipsyncd --show-pin`). A resposta de
> `pair_challenge` carrega apenas `challenge_id`, `expires_at` e `nonce`
> — **o PIN nunca atravessa o fio**.

### Regras

1. A **primeira** mensagem de uma conexão **deve** ser `hello`.
   O servidor fecha a conexão se receber outra coisa primeiro.
2. Se `device.id` está na lista de confiados do servidor, o pareamento
   é pulado e `pair_ok` é enviado imediatamente.
3. Se `device.id` é `null` (ou desconhecido), o servidor gera um PIN de
   6 dígitos, o **exibe localmente no daemon** (bandeja/tray ou
   `clipsyncd --show-pin`) e envia `pair_challenge` com `challenge_id`,
   `nonce` e `expires_at`. O client **não** recebe o PIN — ele é
   digitado pelo usuário a partir da exibição no daemon.
4. `pair_submit` com PIN correto (digitado) + `challenge_id` + nonce
   corretos → `pair_ok`. PIN errado → `pair_fail` e o servidor fecha a
   conexão.
5. O `device_id` recebido em `pair_ok` **deve** ser persistido pelo
   client (SharedPreferences) e enviado em `hello` nas próximas
   conexões.
6. Se um `device_id` já tem uma sessão ativa e reconecta, a **nova**
   sessão substitui a antiga. A sessão antiga recebe `error` com
   código `superseded` e para de receber broadcasts; o client deve
   fechar a conexão ao recebê-lo.

## Mensagens de clipboard

### Texto

```json
{
  "type": "clipboard_text",
  "v": 1,
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
  "v": 1,
  "mime": "image/png",
  "data_b64": "iVBORw0KGgoAAAANSUhEUgAA…",
  "width": 1080,
  "height": 1920,
  "sha256": "hex-64-chars",
  "origin": "uuid-do-device"
}
```

Limite v0.1: 25 MB por imagem (configurável em `max_image_bytes`).
> Planejado v0.3: transferência via frames binários WebSocket com
> hash + id de transferência, evitando base64 para arquivos grandes.

### HTML (rich text, v0.2)

```json
{
  "type": "clipboard_html",
  "v": 1,
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

O servidor envia `{"type":"ping","v":1}` a cada 30s e espera `pong` do
client. Client que não responder em 60s é considerado morto e
desconectado.

```json
{"type":"ping","v":1}
{"type":"pong","v":1}
```

## Segurança (v0.1)

- WebSocket **sem TLS** na v0.1 — tráfego apenas na LAN confiável.
- Pareamento por PIN de 6 dígitos exibido no daemon (`clipsyncd --show-pin`)
  e digitado no app. O PIN **nunca** é transmitido: `pair_challenge`
  responde apenas com `challenge_id`, `nonce` e `expires_at`; o `code`
  digitado só aparece no `pair_submit` do próprio device que está sendo
  pareado.
- Planejado v0.2: TLS com certificado auto-assinado + pinning, e
  criptografia AES-GCM por mensagem (chave derivada do PIN + salt via
  HKDF/PBKDF2).

## Endpoints

- `ws://<host>:8765/ws` — websocket principal.
- `http://<host>:8765/healthz` — healthcheck (200/ok).
- `http://<host>:8765/` — info JSON do daemon (name, version).

## Detecção na LAN (mDNS)

O daemon anuncia o serviço `_clipsync._tcp.local.` na porta 8765.
O app Android deve fazer *browse* desse serviço (biblioteca `NsdManager`
do Android) para descobrir o daemon sem configuração manual.

Campos TXT:
| Chave        | Valor                       |
|--------------|-----------------------------|
| `name`       | Nome amigável do PC         |
| `protocol`   | `v1`                        |
| `port`       | Porta do websocket          |
| `host`       | IP do daemon                |
