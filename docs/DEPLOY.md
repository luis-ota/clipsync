# Deploy do relay

O processo de produção é o binário `clipsyncd` em modo headless. Não existe um
binário separado chamado `relay`.

## Contrato operacional

- WebSocket: `wss://HOST:8765/ws` (ou a URL publicada pelo proxy).
- Liveness: `GET /healthz` retorna `200` quando o processo responde.
- Readiness: `GET /readyz` retorna `200` enquanto o processo aceita conexões e
  `503` durante shutdown.
- O relay não expõe endpoint de métricas com clipboard. Logs contêm somente
  estado, tipos de evento, tamanhos, contadores e endereços de peer; nunca
  conteúdo, PIN, payload JSON ou imagem.

## Configuração

Comece em [`deploy/config/relay.toml`](../deploy/config/relay.toml). Os nomes
correspondem diretamente aos campos de `Config`; não use a seção `[server]`.
Troque o `device_id` de exemplo por um UUID único por instalação antes do
primeiro start; isso permite montar o TOML como somente leitura.

O arquivo pode ser escolhido por `--config PATH` ou `CLIPSYNC_CONFIG`. Variáveis
operacionais têm precedência sobre o arquivo:

| Variável | Campo |
| --- | --- |
| `CLIPSYNC_BIND` | `bind` |
| `CLIPSYNC_NAME` | `name` |
| `CLIPSYNC_DISCOVERY_ENABLE_MDNS` | `discovery.enable_mdns` |
| `CLIPSYNC_SECURITY_TRANSPORT` | `security.transport` (`tls` ou `plaintext_legacy`) |
| `CLIPSYNC_SECURITY_LOCAL_ONLY` | `security.local_only` |
| `CLIPSYNC_LIMITS_MAX_CONNECTIONS` | `limits.max_connections` |
| `CLIPSYNC_LIMITS_MESSAGES_PER_MINUTE` | `limits.messages_per_minute` |
| `CLIPSYNC_LIMITS_BYTES_PER_MINUTE` | `limits.bytes_per_minute` |

Os limites são por endereço IP. `0` desabilita o respectivo limite. O limite de
payload do WebSocket continua sendo derivado de `clipboard.max_*_bytes`.

Valide uma configuração sem iniciar o processo:

```bash
clipsyncd validate-config --config /etc/clipsync/config.toml
```

## systemd

Crie o usuário/grupo `clipsync`, instale o binário em
`/usr/local/bin/clipsyncd`, o TOML em `/etc/clipsync/config.toml` e o estado em
`/var/lib/clipsync`. Depois:

```bash
sudo install -m 0644 deploy/systemd/clipsyncd.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now clipsyncd
curl --fail http://127.0.0.1:8765/readyz
```

O unit não usa `clipsyncd service-install`: esse comando gera um unit de sessão
do desktop e não é apropriado para um relay headless. O `--no-tray` e o nome do
binário acima são a interface existente do projeto.

## Docker Compose

```bash
cp deploy/.env.example deploy/.env
docker compose -f deploy/docker-compose.yml config
docker compose -f deploy/docker-compose.yml up -d --build
curl --fail http://127.0.0.1:8765/readyz
```

O volume mantém `device_id`, certificado autoassinado e `trusted.toml`. A
imagem roda sem backend de clipboard de desktop; por isso o modo headless
continua disponível para encaminhar mensagens, mas não deve ser usado para
sincronizar o clipboard local do host.

## TLS no reverse proxy

[`deploy/caddy/Caddyfile`](../deploy/caddy/Caddyfile) é um exemplo mínimo de
terminação TLS. Nesse desenho, mantenha o backend isolado na rede privada e
configure o TOML ou ambiente com:

```text
CLIPSYNC_BIND=0.0.0.0:8765
CLIPSYNC_SECURITY_TRANSPORT=plaintext_legacy
```

Isso é aceitável somente no segmento privado entre proxy e container. Para
TLS também no hop interno, mantenha `transport = "tls"`, configure as paths de
certificado no TOML e use `https://clipsyncd:8765` no proxy com validação da CA.
Não publique a porta interna diretamente quando o proxy for a fronteira TLS.

O WebSocket exige que o proxy encaminhe `Upgrade` e `Connection`; Caddy faz
isso automaticamente. O healthcheck deve usar `/readyz`, não `/ws`.

## Observabilidade e redaction

Use `RUST_LOG` para ajustar verbosidade. Em produção, prefira `info` e envie
stdout/stderr para journald ou o coletor do container. Não habilite logs de
payload nem adicione middleware que registre headers/corpo de requisições.
Falhas de protocolo são registradas com o tipo e o tamanho, sem o texto
recebido. Endereços IP são metadados operacionais e podem ser tratados como
dados pessoais pelo operador.

O health/readiness não consulta clipboard, não retorna peers, PIN,
fingerprint, certificados nem configuração. Para confirmar uma alteração de
limite, use `validate-config` e consulte apenas os códigos HTTP do endpoint.
