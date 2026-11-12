# esp32-agent

**Français** · [English](README.md) · [简体中文](README.zh.md) · [Español](README.es.md) · [Русский](README.ru.md) · [العربية](README.ar.md)

Firmware pour un ESP32 qui exécute sa propre boucle d’agent et expose un point de terminaison
MCP : un seul modèle peut ainsi piloter la carte directement, et tout agent externe peut
utiliser les mêmes outils en HTTP.

On le flashe, on rejoint le WiFi, et il sonde les bus pour lesquels il a des pilotes. Chaque
périphérique qui répond devient des outils MCP. L’agent de la carte voit ces mêmes outils et
répond avec eux ; un nouveau capteur peut être déclaré en JSON pendant que la carte tourne.

> La boucle d’agent suit [FutureOS](https://github.com/futuregene/future-os) : transcription,
> journal des outils, événements de flux, adaptateurs de protocole, index de mémoire. Là où le
> firmware diffère (des kilooctets de RAM au lieu de gigaoctets, pas de résumés écrits par le
> modèle), le code dit pourquoi.

| Quoi | En détail |
|---|---|
| Un modèle tourne sur la carte | Boucle, outils, mémoire et transcription sont dans le firmware ; le modèle est n’importe quel point de terminaison compatible OpenAI ou Anthropic. [Donnez-lui un modèle](#donnez-lui-un-modèle) |
| Le jeu d’outils est un document | `manifest_apply` valide un manifeste JSON, le compile en outils, effectif dans le même tour. [Manifeste](#manifeste) |
| Il trouve son propre matériel | L’I2C, le 1-Wire et les broches déclarées sont sondés au démarrage ; seul ce qui répond obtient des outils. [Capacités](docs/capabilities.md) |
| Un seul jeu d’outils, deux appelants | Le point de terminaison MCP et l’agent embarqué voient les mêmes outils, à partir d’une seule description. [API HTTP et MCP](docs/http-api.md) |
| Une console en HTTPS | Discutez et configurez périphériques, outils, planification, journaux, WiFi et modèle depuis un téléphone ; la carte émet son propre certificat. [Console web](#console-web) |
| Il agit sans surveillance | Prompts à heure fixe et règles de capteur qui en déclenchent un quand un relevé franchit un seuil. [Planification](#planification) |
| Un shell et un langage de script | `shell` avec 26 commandes nommées comme dans NuttX, et un interpréteur pour boucler sur des appels d’outils. [Shell](#shell) · [Script](#script) |
| Il retient ce qu'on lui a dit | Consignes permanentes, `/AGENTS.md`, un index `/memory.md` et des compétences, assemblés dans le prompt à chaque tour. [Ce que le modèle voit à chaque tour](#ce-que-le-modèle-voit-à-chaque-tour) |
| Aucun cloud sur le chemin | HTTPS servi par la carte, un certificat qu’elle génère, un seul PIN pour trois appelants. [Authentification](#authentification) |
| Testable sans matériel | Le simulateur exécute le même `agent-core` contre une carte virtuelle. [Simulateur](#simulateur) |

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

## Console web

| Où | Ce que vous y faites |
|---|---|
| Chat (la page) | parlez au modèle ; les appels d’outils et le raisonnement se replient en une ligne par tour |
| Périphériques | ce que la sonde a trouvé, broches, adresses, `Probe devices`, candidats adoptables |
| Outils | chaque outil avec son schéma d’entrée, et pourquoi l’un est masqué |
| Agent | endpoint, model, key, protocol, niveau de réflexion, température, taille du contexte et de la sortie |
| Planification | prompts à heure fixe et règles de capteur |
| Journaux, Réglages | le tampon circulaire ; WiFi avec surcharge de passerelle et de DNS, le PIN, redémarrage |

Aucune étape de construction et rien de chargé depuis Internet : environ 58 KB de HTML, CSS et
JavaScript compilés dedans et servis avec `Cache-Control: no-store`, donc un reflash se voit au
rechargement suivant.

## Donnez-lui un modèle

```bash
curl -sS -X POST http://<board>/api/agent/config -H 'Content-Type: application/json' \
  -d '{"base_url":"https://future-os.cn/api/v1/chat/completions",
       "model":"deepseek-flash","api_key":"<key>"}'
curl -sS -X POST http://<board>/api/agent/message -H 'Content-Type: application/json' \
  -d '{"text":"write HELLO on the OLED, set the LED blue, then scan I2C"}'
```

Trois protocoles : `chat` (complétions de chat OpenAI), `responses` (API Responses
d’OpenAI) et `anthropic` (API Messages). Les images envoyées depuis la console partent comme
parties de contenu. `reasoning` règle le niveau de réflexion, `max_steps` plafonne les appels
au modèle par tour (0 = pas de plafond), `context_tokens` et `max_output_tokens`
règlent la fenêtre et la réserve de réponse, et la transcription se compacte à 80 % de la
fenêtre. Sans clé, pointez `base_url` vers le `/selftest/chat/completions` de la carte et la
boucle tourne quand même.

## Ce que le modèle voit à chaque tour

Quatre couches entrent dans le prompt système, dans cet ordre, à **chaque** tour : ce que le propriétaire a écrit une fois, et ce que l'agent a appris depuis. Aucune ne fait partie du transcript, donc elles coûtent autant pour une conversation d'un message que de mille.

| Couche | Où elle vit | Ce qui parvient au modèle | Qui l'écrit |
|---|---|---|---|
| Consignes permanentes | `agent.system_prompt` (NVS) | tout, plafonné à 2 KB | le propriétaire, par HTTP ou `shell` |
| Instructions du projet | `/AGENTS.md` | le premier 1 KB, puis un repère disant que le reste est dans le fichier | le propriétaire, par `fs_write` |
| Mémoire longue | `/memory.md`, un index pointant vers `/memory/<sujet>.md` | l'index seul, 24 lignes / 2 KB | l'agent lui-même, au fil de ses découvertes |
| Compétences | `/skills/<nom>/SKILL.md` et les fichiers à côté | une ligne chacune, nom et description | l'un ou l'autre |

```bash
# Les consignes du propriétaire : à quoi sert cette carte, et les règles qui passent avant tout.
curl -sS -X POST http://<board>/api/agent/prompt -H 'Content-Type: application/json' \
  -d '{"action":"set","text":"Le relais de la pompe est sur GPIO12. Jamais plus de deux minutes."}'

# Ce que le modèle recevra réellement, les quatre couches comprises : le seul moyen de vérifier.
curl -sS http://<board>/api/agent/prompt | jq -r .system
```

`action` vaut `set`, `append` ou `clear` ; `system_prompt get` la relit depuis la carte et `system_prompt set …` l'écrit du côté de l'agent. La mémoire et les compétences sont des fichiers ordinaires : `fs_write /memory/wiring.md "GPIO18 = ruban"` plus une ligne dans `/memory.md`, et voilà une mémoire ; supprimer le fichier l'oublie. Un fichier modifié prend effet au tour suivant, sans redémarrage, sans reflashage, sans réécriture du transcript.

La raison de ce découpage est la taille : une carte avec quelques kilo-octets de SRAM interne ne peut pas transporter tout ce qu'elle sait dans chaque requête, donc chaque couche est un index ou un préfixe, et le contenu coûteux est lu par `fs_read` quand un tour en a besoin. [docs/agent-memory.md](docs/agent-memory.md) et [docs/agent-skills.md](docs/agent-skills.md) donnent les détails, y compris la façon dont l'index est borné.

## Shell

Chaque pilote du catalogue est documenté dans [docs/capabilities.md](docs/capabilities.md) :
GPIO, ADC, PWM, I2C, 1-Wire, WS2812, SSD1306, BME280, SHT3x, INA219/226, MPU6050, ADS1115,
PCF8574, DS18B20, HC-SR04, servo, relais, micro et ampli I²S.

Une seule zone inscriptible, 8 KB de texte en NVS. `read`, `write` et `edit` couvrent les cas
courants ; tout le reste est une ligne de commande `shell`, analysée avec des mots, des
guillemets et des `--flags`. Un résultat d’outil est plafonné à 2 KB et une réponse à 8 KB : la
carte a 8 MB de PSRAM et quelques kilooctets de SRAM interne, et c’est le second chiffre qui
contraint :

```text
shell {"command":"ls /notes"}            shell {"command":"grep GPIO /"}
shell {"command":"cp /a.txt /b.txt"}     shell {"command":"rm /a.txt --confirm"}
shell {"command":"curl https://example.com/"}          # GET, HTML reduced to text
shell {"command":"wget -o /dl/page.txt https://example.com/"}
shell {"command":"ping -c 3 192.168.3.1"}   shell {"command":"nslookup example.com"}
shell {"command":"ifconfig"}                # configured gateway and DNS, not the lease
shell {"command":"df | free | uptime | date | uname | ps"}
shell {"command":"sha256sum /notes/a.txt"}  shell {"command":"zip t.zip /a.txt"}
```

`edit` refuse une correspondance ambiguë plutôt que de réécrire le mauvais endroit ; `rm` et `mv`
exigent `confirm`. Une commande fausse répond par sa ligne d’usage, tirée de la même table que le
texte d’aide, donc les deux ne peuvent pas diverger : `rm without --confirm` renvoie
`usage: rm <path> --confirm`. `shell {"command":"help"}` liste les 26.

## Script

Pour ce que les outils fixes ne peuvent pas exprimer : une boucle sur des relevés, un
branchement sur un résultat.

```text
let scan = tool("i2c_scan", {})
let i = 0
while i < len(scan.addresses) { log(scan.addresses[i])  i = i + 1 }
if i > 0 { "found " + str(i) } else { "nothing" }
```

Nombres, chaînes, booléens, nil, listes, tables ; `let`, affectation, `if`/`else`, `while`,
`break` ; fonctions intégrées `len`, `str`, `num`, `type`, `log` et `tool(name, args)`, qui
appelle n’importe quel outil de la carte avec les permissions de l’appelant (`tool("script")`
excepté). `{"lint":true}` vérifie la syntaxe et les noms inconnus avec ligne et colonne et
n’exécute jamais ; `{"help":true}` renvoie la référence. Bornes : 30 s, cinq millions d’étapes,
32 blocs imbriqués, chaînes de 4 KB, 8 KB de source. L’horloge est la vraie limite :
l’interpréteur tourne sur le fil qui répond à la console.

## Planification

```bash
curl -sS -X POST http://<board>/api/sched -H 'Content-Type: application/json' -d '{
  "tasks":[{"id":"t1","prompt":"read the BME280","every_s":300}],
  "rules":[{"id":"r1","tool":"bme280_read","path":"temperature_c","op":"gt","value":30,"prompt":"it is hot"}]}'
```

`at_hhmm` déclenche plutôt à une heure d’horloge, en UTC, et chaque réponse arrive dans la
transcription marquée `source: "schedule:t1"`. L’agent peut se planifier lui-même avec
`schedule {"action":"add", …}`. Les tâches survivent à un redémarrage.

## Manifeste

Un périphérique qui n’a besoin ni de minutage ni de calibration se déclare, il ne se
compile pas :

```json
{"devices":[{"id":"pump","kind":"actuator","pins":{"out":12},"note":"active-low relay"}],
 "tools":[{"name":"pump_pulse","description":"Energise the relay for `ms` ms",
   "params":[{"name":"ms","type":"integer","min":10,"max":5000,"required":true}],
   "exec":{"type":"sequence","steps":[
     {"ms":0,"exec":{"type":"gpio_write","pin":12,"level":0}},
     {"ms":0,"exec":{"type":"delay","ms":{"param":"ms"}}},
     {"ms":0,"exec":{"type":"gpio_write","pin":12,"level":1}}]},
   "safety":{"max_calls_per_min":20,"confirm":true}}]}
```

Trois chemins ajoutent du matériel : un manifeste (sans minutage ni machine à états, appliqué à
chaud), un pilote compilé (`drivers/<chip>.rs`, pour les machines à états et le minutage strict),
ou un profil de carte (les faits qui ne peuvent pas être sondés, comme les broches de la LED et
des boutons). Un pilote n’est listé que lorsque la sonde trouve le périphérique, donc un pilote à
moitié écrit ne peut pas annoncer du matériel absent. La validation s’exécute sur la carte : les
plages de paramètres et un plafond par minute `safety` sont vérifiés avant qu’une broche ne bouge,
et un appel hors plage renvoie la plage qu’il accepterait.
[docs/adding-a-device.md](docs/adding-a-device.md) couvre les trois.

## Authentification

| Appelant | Identifiant |
|---|---|
| Console | `POST /api/auth {"pin":"…"}` renvoie un jeton bearer que la page conserve |
| Script | `X-Portal-Pin: <pin>`, ou le même jeton |
| Client MCP | `Authorization: Bearer <mcp.token>` ; le PIN est accepté quand aucun jeton n’est défini |

Quand `auth.portal_pin` est défini, les écritures et les lectures qui exposent la transcription,
le prompt, la configuration et les journaux l’exigent ; statut, périphériques, outils et
`/agent.json` restent ouverts. Les jetons vivent en RAM, donc un redémarrage les révoque. Par
défaut il n’y a pas de PIN, et `/agent.json` le dit dans son bloc `security`.

## Simulateur

```bash
cargo run -p esp32-agent-sim
```

Le même `agent-core` contre une carte virtuelle sur un port réel. Seule l’implémentation de
`Hardware` diffère, donc la console, le point de terminaison MCP, les pilotes, le moteur de
manifeste et la boucle d’agent sont le code qui tourne sur la puce.

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
| `agent-core` | 424 tests unitaires et 14 tests d’intégration passent ; `scripts/smoke.ps1` passe de bout en bout |
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
[Skills](docs/agent-skills.md) · [Mémoire](docs/agent-memory.md) · [Scripting](docs/scripting.md)

## Contribuer

Un nouveau périphérique commence comme fragment de manifeste sous `examples/manifests/` ; un
pilote Rust n’est nécessaire que pour le minutage, la calibration ou l’état. `CATALOG` est
l’endroit où un pilote s’enregistre. Les conventions sont dans [AGENTS.md](AGENTS.md) et
[CONTRIBUTING.md](CONTRIBUTING.md).

## Licence

MIT. Voir [LICENSE](LICENSE).
