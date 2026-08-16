# Contribuindo

Obrigado por querer contribuir com o `clipsync`! PRs são bem-vindos —
bugfixes, features, docs, testes, qualquer coisa. Este guia existe para
deixar a contribuição o mais suave possível.

O projeto é um Cargo workspace com dois crates:

```
clipsync/
├── crates/
│   ├── clipsync-core/   # biblioteca: protocolo, WS, mDNS, clipboard, pairing
│   └── clipsyncd/       # binário do daemon (CLI + tray)
├── docs/                # ARCHITECTURE.md, PROTOCOL.md, ANDROID.md
└── .github/workflows/   # CI
```

## Setup

### Pré-requisitos

- [Rust](https://rustup.rs/) (stable; o workspace usa `rust-version = "1.75"`)
- Dependências de runtime do clipboard, dependendo do seu ambiente:

```bash
# Wayland
sudo pacman -S wl-clipboard

# X11 (o daemon usa xclip automaticamente)
sudo pacman -S xclip
```

### Clone e build

```bash
git clone https://github.com/luis-ota/clipsync.git
cd clipsync
cargo build --workspace
```

### Rodar o daemon em desenvolvimento

```bash
# Direto do workspace (stdout com logs e PIN)
cargo run --bin clipsyncd -- run

# Ou instalar o binário uma vez
cargo install --path crates/clipsyncd
clipsyncd run
```

## Workflow

1. **Crie uma branch** a partir de `main` com um prefixo descritivo:
   - `fix/` — correção de bug
   - `feat/` — nova funcionalidade
   - `docs/` — documentação (ex.: `docs/contributing`)
   - `chore/` — manutenção, CI, dependências

2. **Commits no estilo conventional commits**, com resumo em português,
   seguindo os exemplos do histórico do repo:

   ```
   docs: criar CONTRIBUTING.md e templates de issue/PR
   fix: tratar reenvio de clipboard no anti-eco
   feat: adicionar suporte a HTML rich text
   chore: atualizar dependência mdns-sd
   style: cargo fmt
   ```

   Use o corpo do commit para explicar o *porquê* quando necessário.
   Não adicione trailers de atribuição gerados por IA (ex.:
   "Generated with ...") — o autor é você.

3. **Um PR por issue.** Abra um PR no GitHub referenciando a issue
   relacionada (ex.: `Closes #7`) e descreva o que mudou usando o
   [template de PR](.github/PULL_REQUEST_TEMPLATE.md).

4. **Atualize os docs.** Se o protocolo wire mudar, atualize
   [`docs/PROTOCOL.md`](docs/PROTOCOL.md) no mesmo PR — nunca em um
   PR separado.

## Quality gates

Tudo abaixo precisa passar antes de merge. O CI
([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) roda exatamente
estes comandos em `ubuntu-latest`, com `RUSTFLAGS=-D warnings`, além de um
job de audit de dependências:

```bash
# 1. Formatação
cargo fmt --all -- --check

# 2. Clippy sem warnings (zero warnings é obrigatório)
cargo clippy --workspace --all-targets --no-deps

# 3. Testes
cargo test --workspace

# 4. Build completo (binários + testes)
cargo build --workspace --all-targets
```

Pode rodar o mesmo comando que o CI usa para clippy com warnings
virando erro, que é o que você vai ver no CI de qualquer forma:

```bash
RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets --no-deps
```

## Código de conduta

- **Seja gentil.** Todos estão aqui para aprender e colaborar.
- **Zero assédio** — discriminação, assédio ou xingamentos não são
  tolerados em issues, PRs ou comentários.
- **Sem trailers de IA nos commits.** Não adicione atribuição automática
  a ferramentas de IA nas mensagens de commit; o trabalho é seu.
