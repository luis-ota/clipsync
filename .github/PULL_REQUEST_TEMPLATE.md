## Issue relacionada

Resolve/closes: #<!-- número da issue -->

## Resumo das mudanças

Descreva o que este PR faz e por quê. Se for uma correção, explique a
causa raiz; se for uma feature, explique o comportamento novo.

## Protocolo

Esta mudança altera o protocolo wire (`docs/PROTOCOL.md`)?

- [ ] Sim — `docs/PROTOCOL.md` foi atualizado neste PR
- [ ] Não

## Testes realizados

Rodei localmente (marque o que aplica):

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --no-deps` (zero warnings)
- [ ] `cargo test --workspace`
- [ ] `cargo build --workspace --all-targets`

Testes manuais (ex.: clipboard PC → celular → PC com dois peers):

```
[descreva o que testou e o resultado]
```

## Checklist

- [ ] Um PR por issue, branch com prefixo `fix/`, `feat/`, `docs/` ou `chore/`
- [ ] Commits no estilo conventional commits, resumo em português
- [ ] Sem trailers de atribuição gerados por IA nos commits
- [ ] Docs atualizados quando aplicável (`docs/ARCHITECTURE.md`,
      `docs/PROTOCOL.md`, `docs/ANDROID.md`)
