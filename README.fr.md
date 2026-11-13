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
| Le jeu d’outils est un document | `manifest` (`command: apply`) valide un manifeste JSON, le compile en outils, effectif dans le même tour. [Manifeste](docs/manifest.md) |
| Il trouve son propre matériel | L’I2C, le 1-Wire et les broches déclarées sont sondés au démarrage ; seul ce qui répond obtient des outils. [Capacités](docs/capabilities.md) |
| Un seul jeu d’outils, deux appelants | Le point de terminaison MCP et l’agent embarqué voient les mêmes outils, à partir d’une seule description. [API HTTP et MCP](docs/http-api.md) |
| Une console en HTTPS | Discutez avec le modèle et configurez les périphériques, outils, planification, journaux, WiFi et modèle depuis un téléphone ; la carte émet son propre certificat. [Console web](docs/web-console.md) |
| Il agit sans surveillance | Prompts à heure fixe et règles de capteur qui en déclenchent une invite quand un relevé franchit un seuil. [Planification](docs/scheduling.md) |
| Un shell et un langage de script | `shell` avec 57 commandes nommées comme dans NuttX, et un interpréteur pour boucler sur des appels d’outils. [Shell](docs/shell.md) · [Script](docs/scripting.md) |
| Il retient ce qu'on lui a dit | Consignes permanentes, `/AGENTS.md`, un index `/MEMORY.md` et des compétences, assemblés dans le prompt à chaque tour. [Ce que le modèle voit à chaque tour](docs/context.md) |
| Compétences intégrées | Une seule procédure est livrée dans le firmware, `create-skill`, avec l'en-tête standard `---`, et elle enseigne le reste : ce qu'est une compétence et comment en écrire une. Elle arrive dans `/skills` au premier démarrage, et ta modification reste. [Compétences](docs/agent-skills.md) |
| La carte est le serveur | HTTPS servi par la carte, un certificat qu’elle génère, un seul PIN pour trois appelants. [Authentification](docs/security.md) |
| Testable sans matériel | Le simulateur exécute le même `agent-core` contre une carte virtuelle. [Simulateur](docs/simulator.md) |

## Flasher et connecter

```bash
espflash write-bin --chip esp32s3 --flash-size 16mb 0x0 esp32-agent-vX.Y.Z.bin
```

La carte n'a aucun identifiant WiFi : elle démarre un point d'accès `esp32-agent-setup`.
Rejoignez-le, ouvrez `http://192.168.4.1/`, saisissez le nom du réseau et le mot de passe,
enregistrez. Ils vont en NVS et la carte se reconnecte. Avant cela elle n'atteint rien hors
du réseau local : ni point d'entrée de modèle, ni miroir de paquets.

## Utilisation

**Dans un navigateur** : `http://<board>/` (ou `https://<board>/`) est la console. Parler au
modèle, lister les appareils trouvés, activer ou couper des outils, lire le journal, définir
le PIN.

**Depuis un agent** : `GET http://<board>/agent.json` renvoie l'interface en JSON — chaque
route avec sa méthode, son corps et son authentification, la surface MCP, la table des
réglages avec types et valeurs par défaut, et les limites. Un agent qui sait récupérer une URL
n'a besoin de rien d'autre.

**Une carte neuve sait déjà apprendre.** `/skills/create-skill` est la seule procédure livrée : elle dit ce qu'est une compétence, l'en-tête `---` qu'elle doit porter, et comment en écrire une que la prochaine session trouvera. Tout le reste — le câblage de ton banc, quoi faire quand une carte semble morte — est une compétence que l'agent écrit puis conserve. C'est un fichier ordinaire de l'espace de travail, installé au premier démarrage partout où le chemin est vide ; une réinitialisation d'usine le remet et `skills.seed_defaults=false` l'empêche. La première phrase utile à une carte fraîchement flashée peut être « note ton câblage ».

## Donnez-lui un modèle

Ouvrez `http://<board>/`, panneau **Agent**, saisissez le point d'entrée, l'identifiant du
modèle et la clé, enregistrez. La clé est en écriture seule : elle va en NVS et se relit
`__set__`. Le même panneau porte `protocol` (`chat`, `responses` ou `anthropic`), `reasoning`,
`context_tokens` et `max_output_tokens`.

**Ou laissez un agent le faire.** Une fois qu'un agent a récupéré `agent.json`, il connaît ces
réglages, leurs types et leurs valeurs par défaut, donc une consigne suffit :

> configure la carte sur `https://your-provider/v1/chat/completions`, modèle `…`, clé `sk-…`,
> réflexion `low`, contexte `64000`, sortie `4096`

L'agent les applique par `POST /api/agent/config` — le même appel que le panneau — puis relit la
configuration pour confirmer. Aucune ligne de commande de votre côté, et la même consigne vaut
pour chaque carte qu'il peut joindre. Sans aucune clé, `base_url` sur
`http://127.0.0.1/selftest/chat/completions` fait tourner la boucle sur la carte elle-même.

## Notes de sécurité

- **Les identifiants restent sur la carte.** Le WiFi passe en NVS par `provision`, jamais dans
  l’image ; `api_key` est en écriture seule et se relit `__set__`.
- **Définissez un PIN avant que la carte ne quitte votre bureau.** `auth.portal_pin` protège
  toutes les écritures et les lectures qui exposent le transcript, le prompt, la configuration
  et les journaux.
- **Comparez le certificat.** Celui de la carte est auto-signé ; son empreinte est sur
  `GET /api/tls`. Le port 80 est ouvert par défaut : un mauvais certificat coûte une URL, pas la carte ; `mcp.plain_http: false` le ferme, et le câble USB devient alors la seule voie de retour.
- **`shell`, `script` et les outils du manifeste bougent des broches.** C’est le but de la carte ;
  le PIN sépare l’agent qui fait son travail d’un passant qui le ferait.

[Security](docs/security.md) dit ce qui n’est délibérément pas défendu.

## Statut

| Partie | État |
|---|---|
| `agent-core` | 753 tests unitaires et 5 tests d’intégration passent ; `scripts/smoke.ps1` passe de bout en bout |
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
[Model](docs/model.md) · [Web console](docs/web-console.md) · [Scheduling](docs/scheduling.md) · [Shell](docs/shell.md) · [Context layers](docs/context.md) · [Security](docs/security.md) · [Simulator](docs/simulator.md) · [Audio](docs/audio.md)

## Contribuer

Un nouveau périphérique commence comme fragment de manifeste sous `examples/manifests/` ; un
pilote Rust n’est nécessaire que pour le minutage, la calibration ou l’état. `CATALOG` est
l’endroit où un pilote s’enregistre. Les conventions sont dans [AGENTS.md](AGENTS.md) et
[CONTRIBUTING.md](CONTRIBUTING.md).

## Licence

MIT. Voir [LICENSE](LICENSE).
