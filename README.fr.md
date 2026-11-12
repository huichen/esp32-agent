# esp32-agent

**Français** · [English](README.md) · [简体中文](README.zh.md) · [Español](README.es.md) · [Русский](README.ru.md) · [العربية](README.ar.md)

Firmware pour un ESP32 qui exécute sa propre boucle d’agent et expose un point de terminaison
MCP : un seul modèle peut ainsi piloter la carte directement, et tout agent externe peut
utiliser les mêmes outils en HTTP.

On le flashe, on rejoint le WiFi, et il sonde les bus pour lesquels il a des pilotes. Chaque
périphérique qui répond devient des outils MCP. L’agent de la carte voit ces mêmes outils et
répond avec eux ; un nouveau capteur peut être déclaré en JSON pendant que la carte tourne.

> La boucle d’agent suit [FutureOS](https://github.com/futuregene/future-os) : transcription,
> journal des outils, événements de flux, adaptateurs de protocole, index de mémoire.

| Quoi | En détail |
|---|---|
| Un modèle tourne sur la carte | Boucle, outils, mémoire et transcription sont dans le firmware ; le modèle est n’importe quel point de terminaison compatible OpenAI ou Anthropic. [Donnez-lui un modèle](docs/model.md) |
| Le jeu d’outils est un document | `manifest_apply` valide un manifeste JSON, le compile en outils, effectif dans le même tour. [Manifeste](docs/manifest.md) |
| Il trouve son propre matériel | L’I2C, le 1-Wire et les broches déclarées sont sondés au démarrage ; seul ce qui répond obtient des outils. [Capacités](docs/capabilities.md) |
| Un seul jeu d’outils, deux appelants | Le point de terminaison MCP et l’agent embarqué voient les mêmes outils, à partir d’une seule description. [API HTTP et MCP](docs/http-api.md) |
| Une console en HTTPS | Discutez avec le modèle et configurez les périphériques, outils, planification, journaux, WiFi et modèle depuis un téléphone ; la carte émet son propre certificat. [Console web](docs/web-console.md) |
| Il agit sans surveillance | Prompts à heure fixe et règles de capteur qui en déclenchent une invite quand un relevé franchit un seuil. [Planification](docs/scheduling.md) |
| Un shell et un langage de script | `shell` avec 26 commandes nommées comme dans NuttX, et un interpréteur pour boucler sur des appels d’outils. [Shell](docs/shell.md) · [Script](docs/scripting.md) |
| Il retient ce qu'on lui a dit | Consignes permanentes, `/AGENTS.md`, un index `/memory.md` et des compétences, assemblés dans le prompt à chaque tour. [Ce que le modèle voit à chaque tour](docs/context.md) |
| Aucun cloud sur le chemin | HTTPS servi par la carte, un certificat qu’elle génère, un seul PIN pour trois appelants. [Authentification](docs/security.md) |
| Testable sans matériel | Le simulateur exécute le même `agent-core` contre une carte virtuelle. [Simulateur](docs/simulator.md) |

## Flasher et connecter

```bash
cargo install espup espflash ldproxy
espup install
powershell scripts/build-firmware.ps1 -Flash -Port COM3
```

Sans identifiants enregistrés, la carte monte son propre point d’accès. Rejoignez-le depuis un
téléphone et ouvrez `http://192.168.4.1/`, ou envoyez `provision <ssid> <password>` par USB. Les
identifiants vont en NVS, jamais dans l’image.

```bash
curl -sS http://<board>/agent.json | jq '.endpoints | length'
curl -sS -X POST http://<board>/mcp -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | jq '.result.tools | length'
curl -k -sS https://<board>/api/tls | jq -r .fingerprint
```

Le port 443 sert les mêmes routes en TLS avec un certificat généré au premier démarrage ; les
navigateurs avertissent, comparez donc l’empreinte. Le port 80 reste ouvert, donc un mauvais
certificat coûte une URL plutôt que la carte. `GET /agent.json` est toute l’interface sous forme
de données : 40 routes avec méthode, corps, retour et authentification, plus chaque réglage
modifiable avec son type et sa valeur par défaut.

## Donnez-lui un modèle

Le modèle est un réglage, pas une compilation :

```bash
curl -sS -X POST http://<board>/api/agent/config -H 'Content-Type: application/json' \
  -d '{"base_url":"https://your-provider/v1/chat/completions","model":"…","api_key":"…"}'
curl -sS -X POST http://<board>/api/agent/message -H 'Content-Type: application/json' \
  -d '{"text":"écris HELLO sur l’OLED et mets la LED en bleu"}'
```

Trois protocoles (`chat`, `responses`, `anthropic`), un niveau de réflexion, une fenêtre de
contexte et un plafond de sortie, dans le panneau Agent de la console ou par le même `POST`.
Sans clé, pointez `base_url` sur `http://127.0.0.1/selftest/chat/completions` : la boucle tourne
quand même. [Model](docs/model.md) donne la liste des champs et les trois protocoles côte à côte.

## Utilisation

| Depuis | Ce que vous obtenez |
|---|---|
| `http://<board>/` sur un téléphone | discuter avec le modèle, voir les appareils et les outils, tout modifier |
| `POST /mcp` depuis un client MCP | les mêmes outils que l’agent de la carte |
| `POST /api/script`, `POST /api/sched` | exécuter un script maintenant, ou le faire tourner sur un minuteur |
| `GET /agent.json` | toute l’interface sous forme de données : routes, réglages, limites |

```bash
curl -sS -X POST http://<board>/mcp -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call",
       "params":{"name":"shell","arguments":{"command":"ls /"}}}'
```

L’agent de la carte peut aussi s’ajouter du matériel (`manifest_apply`), une mémoire
(`write /memory/…`) et un accès shell : mêmes appels, même PIN.

## Notes de sécurité

- **Les identifiants restent sur la carte.** Le WiFi passe en NVS par `provision`, jamais dans
  l’image ; `api_key` est en écriture seule et se relit `__set__`.
- **Définissez un PIN avant que la carte ne quitte votre bureau.** `auth.portal_pin` protège
  toutes les écritures et les lectures qui exposent le transcript, le prompt, la configuration
  et les journaux.
- **Comparez le certificat.** Celui de la carte est auto-signé ; son empreinte est sur
  `GET /api/tls`. Le port 80 reste ouvert exprès : un mauvais certificat coûte une URL, pas la carte.
- **`shell`, `script` et les outils du manifeste bougent des broches.** C’est le but de la carte ;
  le PIN sépare l’agent qui fait son travail d’un passant qui le ferait.

[Security](docs/security.md) dit ce qui n’est délibérément pas défendu.

## Organisation du dépôt

`crates/` se compile et se teste sur un hôte ; `firmware/` est le backend ESP-IDF. `agent-core`
est la carte derrière un seul trait `Hardware`, sans code spécifique à une puce, donc
`cargo test` l’exécute sur un portable ; `agent-sim` implémente ce trait contre un fichier de
registres. Les fusionner mettrait le simulateur dans chaque image et les dépendances du firmware
dans chaque test. `firmware/` reste hors du workspace parce qu’il exige `cargo +esp`,
`-Zbuild-std` et un ESP-IDF extrait.

```
crates/agent-core/    hardware.rs (the trait) · drivers/ (one module per chip) · boards.rs ·
                      manifest.rs · agent.rs + llm.rs · sched.rs · tools/ ·
                      http.rs · app.rs · sitemap.rs + settings.rs · virtual_board.rs
crates/agent-sim/     the virtual board behind an HTTP port
firmware/             main.rs (boot order) · runner.rs (agent thread, TLS) ·
                      esp_hw.rs (GPIO/I2C/PWM/ADC/RMT/I2S) · console.rs · wifi.rs
```

## Statut

| Partie | État |
|---|---|
| `agent-core` | 427 tests unitaires et 14 tests d’intégration passent ; `scripts/smoke.ps1` passe de bout en bout |
| Simulateur | console, MCP et la boucle d’agent fonctionnent de bout en bout |
| Firmware | flashé sur un ESP32-S3-DevKitC-1 N16R8 (IDF v5.5.2) : 42 outils, OLED 128×32, micro et ampli I²S, WS2812, console TLS, tours de modèle en HTTPS |
| Non implémenté | OTA, carte SD, DHT11/22, caméra. Voir `ROADMAP.md` |

Chiffres sur banc : un tour à quatre outils en 13,7 s ; la console en TLS en 0,55 s ; scan I²C
stable sur 8 exécutions ; SRAM interne 5–6 kB libres après une page TLS, la contrainte la plus
serrée ici.

## Documentation

[Architecture](docs/architecture.md) · [Manifeste](docs/manifest.md) · [Capacités](docs/capabilities.md) ·
[HTTP et MCP](docs/http-api.md) · [Ajouter un périphérique](docs/adding-a-device.md) · [Flashage](docs/flashing.md) ·
[Dépannage](docs/troubleshooting.md) · [Boucle d’agent](docs/agent-loop.md) · [Flux](docs/agent-stream.md) ·
[Skills](docs/agent-skills.md) · [Mémoire](docs/agent-memory.md) · [Scripting](docs/scripting.md) ·
[Model](docs/model.md) · [Web console](docs/web-console.md) · [Scheduling](docs/scheduling.md) · [Shell](docs/shell.md) · [Context layers](docs/context.md) · [Security](docs/security.md) · [Simulator](docs/simulator.md) · [Performance](docs/performance.md)

## Contribuer

Un nouveau périphérique commence comme fragment de manifeste sous `examples/manifests/` ; un
pilote Rust n’est nécessaire que pour le minutage, la calibration ou l’état. `CATALOG` est
l’endroit où un pilote s’enregistre. Les conventions sont dans [AGENTS.md](AGENTS.md) et
[CONTRIBUTING.md](CONTRIBUTING.md).

## Licence

MIT. Voir [LICENSE](LICENSE).
