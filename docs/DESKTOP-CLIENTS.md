# Clientes desktop macOS e Windows

`crates/clipsync-client` é o cliente desktop oficial para macOS e Windows para #60
e #62. Ele é um processo foreground, não substitui o `clipsyncd` Linux.

## Escopo validável

- Compila, testa e executa clippy em `macos-latest` e `windows-latest` no CI.
- Faz handshake v1, pareamento por PIN e reconnect com backoff até 30 segundos.
- Persiste `device_id` por `server_id` em `desktop-client.json`; vários servidores
  podem coexistir. Use `--server-id` quando um servidor mudar de endereço.
- Usa `arboard` para texto e imagem RGBA/PNG nas plataformas suportadas.
- Descobre `_clipsync._tcp.local.` via mDNS quando `--url` não é informado.
- Endpoints manuais usam `--url`; `wss://` exige fingerprint SHA-256 do DER.
- Relay usa `--relay-token`/`CLIPSYNC_RELAY_TOKEN` como Bearer somente no handshake.
- O clipboard no relay usa `relay_envelope` AES-256-GCM ponta a ponta. Provisione
  a chave por `e2e_key_ref`/`CLIPSYNC_E2E_KEY_REF`, nunca na URL ou em logs.

## Limitações explícitas

- Não há tray, instalador ou serviço em background.
- `arboard` não expõe uma API portátil de `text/html` para este cliente; HTML
  recebido usa o fallback plain quando existe. A capability HTML permanece falsa.
- Imagens são convertidas para PNG RGBA. Formatos ou buffers que não puderem ser
  decodificados pelo `arboard`/decoder são rejeitados com erro de sessão.
- O cliente não é substituto do `clipsyncd`: o daemon local continua Linux/Wayland/X11.

## Execução

```text
# mDNS + PIN no primeiro uso
clipsync-client --pin 834921

# endpoint manual com pinning
clipsync-client --url wss://192.168.1.50:8765/ws \
  --tls-fingerprint <sha256-do-certificado> --pin 834921

# relay com Bearer (não persistido)
clipsync-client --url wss://relay.example.com/ws \
  --tls-fingerprint <sha256-do-certificado> --relay-token "$CLIPSYNC_RELAY_TOKEN" \
  --e2e-key-ref file:/etc/clipsync/relay-group.key
```

O arquivo contém uma linha `key_id group_id hex_key` por geração e deve ter modo
`0600`; a última linha é a atual. Mantenha as gerações anteriores disponíveis
durante a rotação.

O PIN é gerado e exibido localmente pelo daemon. Ele nunca é enviado no
`pair_challenge`; o usuário deve informar o valor mostrado no daemon/tray. Depois
do primeiro pareamento, o `device_id` é reutilizado somente quando o `server_id`
persistido corresponder. Revogar o device no daemon exige novo PIN.

## Build e instalação

```text
cargo build --release -p clipsync-client
# macOS: target/release/clipsync-client
# Windows: target/release/clipsync-client.exe
```

Distribua o binário com um atalho/serviço da plataforma se desejar início
automático. O repositório não fornece instalador, assinatura de código,
notarização ou configuração de firewall. O estado local contém IDs e endpoints,
não tokens; tokens devem vir do ambiente ou de um secret store.

## CI

O job `desktop-clients` executa fmt, clippy, build release e testes do crate/core
nos runners macOS e Windows. A execução Linux local desta entrega validou apenas
`cargo check -p clipsync-client`; não constitui build ou teste nativo desses
sistemas.
