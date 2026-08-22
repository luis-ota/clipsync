# Clientes desktop macOS e Windows

`crates/clipsync-client` é o cliente desktop mínimo oficial para atacar #60 e
#62 sem fingir que o daemon Linux é um daemon nativo desses sistemas.

## Escopo validável

- Compila em `macos-latest` e `windows-latest` no CI.
- Faz o handshake v1, pareamento por PIN e sincronização bidirecional de texto.
- Usa `arboard`, que integra o clipboard nativo de macOS e Windows.
- Aceita `ws://` e `wss://` conforme o transporte configurado no endpoint.

## Limitações explícitas

- Não há tray, instalador, descoberta mDNS ou serviço em background.
- Não há imagens/HTML; o cliente anuncia apenas `text`.
- A identidade do device ainda não é persistida entre execuções. Sem uma camada
  de armazenamento por plataforma, o cliente pede pareamento novamente.
- O cliente não está declarado como substituto do `clipsyncd`: o daemon de
  clipboard local continua sendo Linux/Wayland/X11.

## Execução

```text
clipsync-client --pin 834921 ws://192.168.1.50:8765/ws
```

O PIN é gerado e exibido localmente pelo daemon. Ele nunca é enviado no
`pair_challenge`; o usuário deve informar o valor mostrado no daemon/tray.

## CI

O job `desktop-clients` executa `cargo check -p clipsync-client` e os testes do
protocolo em runners macOS e Windows. Uma execução Linux local pode validar a
compilação do crate, mas não constitui validação de integração nativa nesses
sistemas.
