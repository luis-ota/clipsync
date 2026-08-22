# clipsync

[![CI](https://github.com/luis-ota/clipsync/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/luis-ota/clipsync/actions/workflows/ci.yml)
[![Release](https://github.com/luis-ota/clipsync/actions/workflows/release.yml/badge.svg)](https://github.com/luis-ota/clipsync/actions/workflows/release.yml)
[![crates.io](https://img.shields.io/crates/v/clipsync-core.svg)](https://crates.io/crates/clipsync-core)
[![docs.rs](https://docs.rs/clipsync-core/badge.svg)](https://docs.rs/clipsync-core)
[![License](https://img.shields.io/badge/license-MIT%20%2F%20Apache--2.0-blue.svg)](LICENSE)

> Secure clipboard synchronization between Android, Linux, macOS and Windows.

Sincronização segura de clipboard entre Android, Linux, macOS e Windows. Funciona
direto na LAN ou através de um relay self-hosted quando os dispositivos estão
fora da mesma rede.

## Instalação rápida

### Linux

O instalador baixa a última release, verifica o checksum SHA-256 e instala os
binários em `~/.local/bin`:

```bash
curl -fsSL https://raw.githubusercontent.com/luis-ota/clipsync/main/install.sh | sh
```

Depois, abra uma nova sessão ou adicione `~/.local/bin` ao `PATH`:

```bash
clipsyncd run
```

Para instalar uma versão específica:

```bash
curl -fsSL https://raw.githubusercontent.com/luis-ota/clipsync/main/install.sh \
  | CLIPSYNC_VERSION=v0.1.2 sh
```

Também existem pacotes `.deb`, `.rpm`, `PKGBUILD`, Flatpak e bundles binários
na página de [Releases](https://github.com/luis-ota/clipsync/releases).

### Android

Baixe `clipsync-android-<versao>-debug.apk` na release. É um APK instalável
para sideload. Ative a instalação de aplicativos da fonte usada pelo Android.

### macOS e Windows

Baixe o binário correspondente na release. O workflow também valida os projetos
de instalação `.pkg` e `.msix`; assinatura e notarização dependem das credenciais
do mantenedor e não são embutidas neste repositório.

## O que está incluído

- Clipboard de texto e imagens na LAN, com descoberta mDNS.
- Pairing por PIN de seis dígitos e identidade persistente por dispositivo.
- Relay TLS com bearer token, grupos, quotas, health/readiness e replay protection.
- E2E AES-256-GCM para payloads que atravessam o relay, com AAD e detecção de tamper.
- Failover `lan`, `relay` ou `auto` no daemon Linux.
- Transferências binárias bounded com chunks de 64 KiB, hash SHA-256 e confirmação Android.
- Android foreground service, IME para inserção explícita e ações na notificação.
- Persistência atômica, anti-eco concorrente e credenciais protegidas por Keychain/DPAPI.

## Projeto

- [Documentação de arquitetura](docs/ARCHITECTURE.md)
- [Protocolo e modelo de ameaças](docs/PROTOCOL.md)
- [Guia de deploy do relay](docs/DEPLOY.md)
- [Releases e downloads](https://github.com/luis-ota/clipsync/releases)
- [Crate `clipsync-core`](https://crates.io/crates/clipsync-core)

## Artefatos da release

Cada tag `vMAJOR.MINOR.PATCH` dispara `.github/workflows/release.yml` e publica:

| Arquivo | Plataforma | Uso |
|---|---|---|
| `clipsync-android-*-debug.apk` | Android | Sideload do app |
| `clipsync_*_amd64.deb` | Debian/Ubuntu | `sudo apt install ./arquivo.deb` |
| `clipsync-*.x86_64.rpm` | Fedora/RHEL | `sudo dnf install ./arquivo.rpm` |
| `clipsync-*-linux-x86_64.tar.gz` | Linux | Bundle sem gerenciador de pacotes |
| `clipsync-macos-*` | macOS | Cliente desktop |
| `clipsync-windows-*.exe` | Windows | Cliente desktop |
| `PKGBUILD` / Flatpak manifest | Arch/Linux | Build nativo da distribuição |
| `SHA256SUMS` | Todas | Verificação dos downloads |

## Uso Linux

```bash
clipsyncd run
clipsyncd show-pin
clipsyncd list-peers
clipsyncd endpoints list
```

Para uma conexão relay:

```bash
clipsyncd endpoints add relay wss://relay.example.com/ws --scope relay \
  --tls-fingerprint <sha256-do-certificado> \
  --credential-ref CLIPSYNC_RELAY_TOKEN
clipsyncd endpoints test relay
```

O segredo é lido de uma variável de ambiente ou de um arquivo com modo `0600`;
nunca é salvo na URL ou no TOML. Use `[security].outbound_route = "auto"` para
tentar LAN antes do relay.

## Relay self-hosted

O relay não acessa clipboard e não persiste conteúdo por padrão. Para iniciar:

```bash
cp deploy/.env.example deploy/.env
deploy/generate-relay-credentials.sh deploy/relay.tokens
docker compose -f deploy/docker-compose.yml up -d --build
curl --fail https://relay.example.com/readyz
```

Consulte [`docs/DEPLOY.md`](docs/DEPLOY.md) para TLS, Caddy, systemd, Docker,
credenciais, rotação e operação. O fingerprint usado por clientes é o do
certificado público apresentado pelo endpoint final.

## Segurança e limites

- TLS é obrigatório por padrão.
- Payloads relay são cifrados ponta a ponta; o relay encaminha ciphertext opaco.
- Replay, origem falsificada, grupos incorretos, tamper e cliente não autorizado são rejeitados.
- Limites de tamanho, conexões, mensagens e bytes são aplicados antes do parse.
- `plaintext_legacy` existe apenas para compatibilidade explícita em redes controladas.

Leia [`docs/PROTOCOL.md`](docs/PROTOCOL.md) para o contrato wire e o modelo de
ameaças.

## Desenvolvimento

Requisitos: Rust estável, JDK 17, Android SDK 35 e ferramentas de clipboard
`wl-clipboard`/`xclip` quando executando com desktop Linux.

```bash
cargo fmt --all -- --check
cargo test --workspace --no-fail-fast
RUSTFLAGS='-D warnings' cargo clippy --workspace --all-targets --no-deps

cd android
./gradlew test lint assembleDebug
```

O CI valida Debian, Ubuntu, Fedora, Arch, Android, macOS e Windows. O build
Android local gera APK debug; uma assinatura de produção deve ser configurada
com secrets do mantenedor no CI.

## Estrutura

```text
crates/clipsync-core/    protocolo, pairing, clipboard, TLS, E2E e transferências
crates/clipsyncd/        daemon Linux e CLI
crates/clipsync-relay/   relay WebSocket self-hosted
crates/clipsync-client/  cliente desktop macOS/Windows
android/                 aplicativo Android Kotlin/Compose
deploy/                  Docker, systemd, Caddy e credenciais
packaging/               deb, rpm, Arch e Flatpak
```

## Licença

MIT OR Apache-2.0.
