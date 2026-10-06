# AI Team — Rust Control Plane

O firmă software AI reutilizabilă, scrisă în Rust. Planul complet, pe faze, este în
[firma.md](firma.md); acest README descrie ce funcționează acum (fazele 1–4 și 5a).

```text
OWNER (uman)
  └─ EMP-ORCH-001  Orchestrator (control plane, cod)
       ├─ EMP-PLAN-001  Planner     propune echipa pentru fiecare rulare
       ├─ EMP-ARCH-001  Architect   specificația
       ├─ EMP-BUILD-001 Builder     implementarea
       └─ EMP-REV-001   Reviewer    controlul calității
```

Subagenții Python vor fi conectați ulterior (Worker Gateway), fără să schimbăm contractele
orchestratorului.

## Principii

- Orchestratorul are două componente: **control plane** (cod determinist, decide toate
  tranzițiile) și **Planner** (agent AI care doar propune). Un plan invalid este respins, nu executat.
- Angajații AI sunt fișiere în `company/employees/`; adăugarea unui angajat nu cere recompilare.
- Permisiunile sunt acțiuni impuse de platformă, nu descrieri în prompt.
- Reviewer-ul poate întoarce doar `APPROVED` sau `CHANGES_REQUIRED`. Un răspuns care nu e JSON valid
  nu se interpretează: Reviewer-ul este întrebat din nou (de cel mult 2 ori), apoi rularea se oprește.
  Un răspuns invalid nu devine niciodată aprobare.
- La `CHANGES_REQUIRED`, Reviewer-ul spune cine corectează (`"target": "builder"` sau `"architect"`).
  Specificația poate fi trimisă înapoi la Architect de cel mult `max_architecture_revisions` ori (implicit 1).
- Builder-ul își primește implementarea anterioară + feedback-ul, deci corectează, nu rescrie de la zero.
- La depășirea numărului maxim de iterații, taskul devine `HUMAN_REVIEW_REQUIRED`.
- Orice eroare (LLM căzut, protocol încălcat, echipă invalidă) se scrie în audit ca `RUN_FAILED`.
- Secretele se citesc din variabile de mediu, nu din fișierele de configurare.
- Fiecare rulare produce audit în `.ai-team/runs/`; deciziile de personal în `.ai-team/hr.jsonl`.

## Structură

```text
ai-team-rust/
├── company/
│   ├── employees/           # registrul de angajați
│   │   └── EMP-XXX-001/
│   │       ├── contract.yaml       # contractul operațional
│   │       └── job_description.md  # fișa postului = prompt-ul de sistem al agentului
│   ├── proposals/           # propuneri de angajare, în așteptarea aprobării Owner-ului
│   ├── skills/<id>/SKILL.md # pachete de instrucțiuni (skill-uri)
│   ├── mcp.yaml             # serverele MCP ale firmei
│   └── mcp/                 # servere MCP locale (pydoc_server.py)
├── projects/                # lucrările firmei (JSON)
├── scripts/
├── src/
│   ├── agents/              # trait Agent + Architect, Builder/lead, Reviewer/consilieri, Specialist, tool calling
│   ├── registry/            # registru, validare, procedura de angajare
│   ├── planner.rs           # Planner-ul (propune echipa și planul de taskuri)
│   ├── project/             # Faza 3: plan, SQLite + artefacte, executor, decizii Owner
│   ├── workspace/           # Faza 4: fișiere din răspuns, workspace-uri, rularea testelor
│   ├── workbench.rs         # Faza 4: porțile verificare -> review -> îmbinare
│   ├── staffing.rs          # control plane: validează echipa, construiește agenții
│   ├── orchestrator.rs      # workflow-ul
│   ├── llm.rs               # provideri: ollama, openai, claude (timeout, retry, tool calling, consum)
│   ├── mcp.rs               # client MCP (stdio) + Toolbox per angajat
│   ├── audit.rs  config.rs  domain.rs  main.rs  lib.rs
└── tests/
    ├── registry.rs          # regulile firmei, angajare, status
    ├── workflow.rs          # fluxul, staffing, subagenți
    ├── projects.rs          # plan, execuție, reluare, decizii Owner, artefacte, rapoarte
    ├── execution.rs         # fișiere, teste ca poartă, îmbinare cu conflicte, permisiuni, consum
    ├── subagents.rs         # skill-uri, MCP, tool calling, delegare, veto
    └── http_provider.rs     # retry / timeout pe un server HTTP local
```

## Instalare

```powershell
cd C:\dev\ai-team-rust
.\scripts\bootstrap.ps1
cargo build
cargo test
```

Pe Windows cu toolchain-ul `x86_64-pc-windows-gnu`, `C:\msys64\ucrt64\bin` trebuie să fie în `PATH`
(altfel compilarea pică cu „dlltool.exe: program not found”).

## Comenzi

```powershell
cargo run -- doctor     # validează registrul + verifică serverul de modele
cargo run -- team       # organigrama
cargo run -- run --project projects\voice-agent-core.json --task "Definește arhitectura inițială"   # task ad-hoc, nepersistat
```

Opțiunea globală `--company <dir>` (implicit `company`) alege altă firmă.

Firma livrată (`company/`) folosește serverul Ollama (vezi „Folosirea unui LLM real”). `doctor`
verifică și serverul: că răspunde, că fiecare model cerut e instalat și că `num_ctx` nu depășește
contextul maxim al modelului (`doctor --offline` sare peste verificările de rețea).

O rulare reală arată ciclul:

```text
Planner -> echipa propusă -> control plane o acceptă
Architect -> Builder -> Reviewer (CHANGES_REQUIRED -> Builder din nou ...) -> APPROVED -> DONE
```

Testele automate nu folosesc rețeaua: injectează în cod un model fals (doar în `tests/`), pe care
niciun contract nu îl poate cere.

## Consola Owner-ului

```powershell
cargo run -- console                     # pornește cu un proiect nou
cargo run -- console --project <id>      # continuă un proiect existent
```

```text
=== proiect nou ===
> Scrie un modul Python saluturi.py cu funcția salut(nume) care întoarce 'Salut, <nume>!'.
  Adaugă teste unittest care citesc numele din inputs/nume.txt.
                                           <- rând gol = trimite cererea
Fișiere pentru echipa (cale sau trage fișierul aici; Enter gol = gata):
  fisier> "C:\Users\...\nume.txt"         <- poți trage fișierul în fereastră
  fisier>
Comanda de test (Enter = fără teste): python -m unittest discover -s tests -v
Pornesc echipa? [D/n]
  [proiect-T01] architect: specification ready     <- progresul, în timp real
  [proiect-T01] builder: iteration 1 ready
  [proiect-T01] platform: verification passed (3 file(s), tests green)
  [proiect-T01] reviewer: APPROVED
  ...
```

- cererea poate avea mai multe rânduri; un rând gol o trimite;
- fișierele (inclusiv căi cu ghilimele, cum le pune Windows când tragi un fișier) ajung în
  `inputs/` din proiect;
- după prima cerere rămâi în același proiect: următoarea cerere devine un task nou al lui;
- un task oprit (teste roșii, veto) te întreabă pe loc: **[r]** reia cu o notă pentru echipă,
  **[a]** acceptă, **[c]** anulează, Enter = lasă-l așa;
- comenzi: `/nou [nume]` (proiect nou, ex. `/nou ulise`), `/proiect <id>`, `/status`, `/ajutor`, `/iesire`.

## Cerere rapidă a Owner-ului

Același lucru, dintr-o singură comandă (pentru scripturi): scrii ce vrei, atașezi fișiere,
iar echipa se pune pe treabă imediat.

```powershell
cargo run -- request "Scrie un modul raport.py care calculează totalul pe categorie din cheltuieli.csv, cu teste" `
  --file C:\cale\cheltuieli.csv `
  --test-command "python -m unittest discover -s tests -v"
```

- se creează un proiect nou din textul tău (obiectivul = cererea ta);
- fișierele se copiază în repository-ul proiectului, în `inputs/`, cu un commit al Owner-ului;
  toată echipa vede lista lor și primele rânduri din fiecare fișier text;
- cererea devine direct un task — instrucțiunea ta e aprobarea, fără plan intermediar — și trece
  prin tot fluxul: Architect → Builder (cu subagenți) → teste → Reviewer (cu Security) → merge;
- la final vezi statusul, fișierele produse și unde sunt (`data/workspaces/<proiect>`, branch `main`).

Opțiuni:

| Opțiune | Efect |
|---|---|
| `--file <cale>` | atașează un fișier (se poate repeta; maximum 10 MB/fișier) |
| `--project <id>` | adaugă cererea ca task nou într-un proiect existent (și îl redeschide dacă era gata) |
| `--test-command "..."` | comanda care trebuie să treacă după fiecare răspuns al Builder-ului |
| `--plan` | Planner-ul împarte cererea în mai multe taskuri, pe care le aprobi înainte de start |
| `--yes` | aprobă planul fără întrebare (cu `--plan`) |

Un task oprit (teste care nu trec, veto) așteaptă decizia ta, ca orice alt task: `task show`,
`task resume --note "..."`, `task accept`, `task cancel`.

## Registrul de angajați

Exemplu de contract (`company/employees/EMP-BUILD-001/contract.yaml`):

```yaml
employee_id: EMP-BUILD-001
name: Builder
title: Head of Engineering
department: engineering
manager_id: EMP-ORCH-001
employee_type: agent          # system | agent | subagent
function: builder             # orchestrator | planner | architect | builder | reviewer | specialist
status: active                # active | suspended | disabled
model:
  provider: ollama
  model: qwen3-coder:30b
  base_url: http://10.10.0.14:11434
skills: [implementation, testing]
permissions:
  - write_implementation
  - delegate_subtasks
forbidden_actions:            # informativ: ce nu e în permissions e oricum interzis
  - approve_own_work
```

Permisiunile existente (doar cele pe care platforma le impune deja):

| Permisiune | Necesară pentru |
|---|---|
| `propose_plan` | Planner (doar el o poate avea) |
| `write_specification` | Architect |
| `write_implementation` | Builder |
| `review_work` | Reviewer |
| `delegate_subtasks` | a avea subagenți |
| `write_workspace` | platforma scrie răspunsurile în workspace (cere `write_implementation`) |
| `run_tests` | platforma rulează comanda de test pe muncă (cere `write_implementation`) |
| `veto_review` | consilier al Reviewer-ului al cărui „nu” nu poate fi anulat (cere `review_work`) |


Registrul e validat la fiecare încărcare; toate erorile sunt raportate deodată. Reguli principale:

- exact un orchestrator (`system`, raportează la `OWNER`, fără model), cel mult un Planner activ;
- agenții raportează la orchestrator, subagenții la un agent care are `delegate_subtasks`;
- un subagent nu poate avea permisiuni pe care managerul nu le are și nu poate delega mai departe;
- nimeni nu poate avea `write_implementation` și `review_work` împreună (nu-și aprobă propria muncă);
- fiecare funcție are permisiunea ei; numele folderului = `employee_id`; câmpurile necunoscute
  (greșeli de tipar) sunt respinse.

## Echipa unei rulări

1. Dacă proiectul are `assigned_team`, se folosește aceasta (decizia Owner-ului):

   ```json
   "assigned_team": { "architect": "EMP-ARCH-001", "builder": "EMP-BUILD-001", "reviewer": "EMP-REV-001" }
   ```

2. Altfel, Planner-ul primește lista angajaților activi și propune echipa.
3. Control plane-ul verifică: angajat existent, activ, cu funcția și permisiunea potrivite. Un plan
   respins (`PLAN_REJECTED`) nu se execută; motivul este trimis înapoi Planner-ului, care propune
   din nou — de cel mult 3 ori. După a treia respingere rularea se oprește (`RUN_FAILED`), fără ca
   vreun agent să lucreze. O echipă din `assigned_team` (decizia Owner-ului) nu se renegociază:
   dacă e invalidă, rularea se oprește direct.
4. Dacă Builder-ul și Reviewer-ul folosesc același model, rularea continuă, dar se înregistrează un
   `POLICY_WARNING`.

## Angajarea unui angajat nou

```powershell
cargo run -- hire propose EMP-PY-001 --name "Python Developer" --function specialist --manager EMP-BUILD-001
# completează toate TODO din company\proposals\EMP-PY-001\
cargo run -- hire check EMP-PY-001
cargo run -- hire approve EMP-PY-001
```

`propose` creează scheletul (modelul e copiat de la manager, permisiunea e cea a departamentului).
`approve` refuză o propunere care mai conține `TODO` sau care ar face registrul invalid; doar după
aprobare angajatul apare în `company/employees/`.

Suspendare / reactivare (comentariile din contract se păstrează):

```powershell
cargo run -- employees set-status EMP-PY-001 suspended
```

## Folosirea unui LLM real

Fiecare angajat are blocul `model:` în contract. Dialecte (`provider`):

| `provider` | Endpoint | Autentificare | Opțiuni specifice |
|---|---|---|---|
| `ollama` | `{base_url}/api/chat` (API nativ) | — | `num_ctx`, `num_predict` |
| `openai` | `{base_url}/chat/completions` | `Authorization: Bearer` din `api_key_env` | `max_tokens` |
| `claude` | `{base_url}/v1/messages` (implicit `https://api.anthropic.com`) | `x-api-key` din `api_key_env` (implicit `ANTHROPIC_API_KEY`) | `max_tokens` (implicit 16000), `effort` |

Configurația curentă a echipei (server Ollama `http://10.10.0.14:11434`):

| Angajat | Model | `num_ctx` | `num_predict` |
|---|---|---|---|
| Planner | `qwen3-coder:30b` | 32768 | 4096 (un plan de proiect în JSON) |
| Architect, Builder | `qwen3-coder:30b` | 32768 | 8192 (specificații și implementări pe taskuri reale) |
| Reviewer | `qwen2.5:14b` (altă familie decât Builder-ul → verificare independentă) | 32768 | 1200 |

Toți: `temperature: 0.1`, `timeout_secs: 180`. De ce `num_ctx: 32768` și nu 65536: la 65536
`qwen3-coder:30b` nu mai încape în GPU și scade de la ~147 la ~7 tokeni/s; `qwen2.5:14b` are oricum
contextul maxim 32768. Modelele cu „thinking” (`qwen3:4b`, `qwen3.6`) nu sunt folosite: gândirea
consumă `num_predict` și nu mai ajung la răspunsul JSON.

Exemplu de bloc `model:`:

```yaml
model:
  provider: ollama
  model: qwen3-coder:30b
  base_url: http://10.10.0.14:11434   # fără /v1
  temperature: 0.1
  num_ctx: 32768
  num_predict: 8192
  timeout_secs: 180
```

Pentru Ollama se folosește API-ul nativ, nu cel compatibil OpenAI: endpoint-ul `/v1` al Ollama
ignoră `num_ctx`, iar prompturile ar fi trunchiate tăcut la fereastra implicită a serverului.

Exemplu Claude:

```yaml
model:
  provider: claude
  model: claude-opus-5-5
  effort: high          # low | medium | high | xhigh | max
  max_tokens: 16000
```

`temperature` nu se trimite la `claude` (modelele actuale resping parametrii de eșantionare);
calitatea/costul se reglează din `effort`. Un răspuns `refusal` este tratat ca eroare.

**Răspunsurile tăiate sunt erori în toate dialectele** (`done_reason: length` la Ollama,
`finish_reason: length` la OpenAI, `stop_reason: max_tokens` la Claude): o specificație sau o
implementare neterminată nu ajunge la Reviewer. Dacă apare eroarea „cut off at num_predict”,
mărește `num_predict` (Ollama) sau `max_tokens` (OpenAI/Claude) în contractul angajatului.

Opțiuni comune: `timeout_secs` (implicit 120), `max_retries` (implicit 2), `retry_backoff_ms`
(implicit 1000). Se reîncearcă doar erorile trecătoare (timeout, conexiune, 429, 5xx), cu pauza
dublată la fiecare încercare; erorile 4xx (cheie greșită, model inexistent) opresc imediat.
O opțiune pe care providerul ales n-o suportă (ex. `num_ctx` la `claude`) face contractul invalid.
Vechiul nume `openai_compatible` este acceptat ca alias pentru `openai`.

## Proiecte și taskuri (Faza 3)

Un proiect se descompune în milestone-uri și taskuri cu dependențe; starea fiecărui task se
salvează după fiecare pas în `data/` (SQLite + artefacte), deci o rulare întreruptă continuă de unde
a rămas, iar un task oprit așteaptă decizia Owner-ului.

```powershell
cargo run -- project add projects\voice-agent-core.json   # înregistrează proiectul
cargo run -- project plan voice-agent-core                 # Planner-ul propune planul (încă neaprobat)
cargo run -- project plan voice-agent-core --note "mai puține taskuri"   # cere altă propunere
cargo run -- project approve voice-agent-core              # Owner-ul aprobă -> se creează taskurile
cargo run -- project run voice-agent-core                  # execută taskurile gata, unul câte unul
cargo run -- project run voice-agent-core --max-tasks 2    # ... sau doar câteva
cargo run -- project status voice-agent-core               # milestone-uri, taskuri, ce așteaptă Owner-ul
cargo run -- task show voice-agent-core-T03                # detalii, ultimul review, artefacte, rulări
```

Cum funcționează:

1. **Plan.** Planner-ul propune milestone-uri și taskuri (cheie, titlu, descriere, criterii de
   acceptare, `depends_on`). Control plane-ul verifică: maximum 25 de taskuri, chei unice, fiecare
   task cu cel puțin un criteriu, dependențe existente, doar spre același milestone sau unul
   anterior, fără cicluri. Un plan invalid e trimis înapoi Planner-ului cu motivul (de cel mult 3 ori)
   și nu se salvează. Planul valid rămâne **propus** până la `project approve`.
2. **Execuție.** `project run` ia, pe rând, primul task ale cărui dependențe sunt toate `DONE`
   (taskurile întrerupte au prioritate) și rulează pe el fluxul Planner (echipa) → Architect →
   Builder ⇄ Reviewer. Agenții primesc: taskul, criteriile lui, criteriile proiectului (ca context),
   implementarea aprobată a dependențelor (primele 6000 de caractere) și instrucțiunile Owner-ului.
3. **Stări** (firma.md §13): `PLANNED` → `ASSIGNED` → `IN_PROGRESS` → `REVIEW` ⇄
   `CHANGES_REQUIRED` → `DONE`; opriri: `HUMAN_REVIEW_REQUIRED` (n-a trecut de review în bugetul
   de iterații), `FAILED` (eroare tehnică), `CANCELLED`. Un task ale cărui dependențe s-au oprit
   devine `BLOCKED` și revine la `PLANNED` când se deblochează.
4. **Decizii Owner** pentru taskurile oprite:

   ```powershell
   cargo run -- task resume voice-agent-core-T03 --note "folosește UUID-uri" --iterations 2
   cargo run -- task accept voice-agent-core-T03 --note "suficient pentru acum"
   cargo run -- task cancel voice-agent-core-T03 --note "nu mai e necesar"
   ```

   `resume` continuă taskul din starea salvată (nu de la zero), cu iterații suplimentare și nota
   ta inclusă în prompturi; `accept` marchează taskul `DONE` ca aprobare manuală (raportul o arată
   distinct de o aprobare a Reviewer-ului); `cancel` lasă dependenții blocați. Toate deciziile se
   înregistrează în baza de date.
5. **Rapoarte.** Când toate taskurile unui milestone sunt `DONE` sau `CANCELLED`, se scrie
   `data/reports/<proiect>-M<n>.md` (echipă, iterații, review final, artefacte, decizii Owner).

Stocare (`--data <dir>`, implicit `data/`, în `.gitignore`):

- `ai-team.db` — proiecte, planuri, milestone-uri, taskuri, dependențe, rulări, decizii Owner;
- `artifacts/<sha256>.md` — specificațiile și implementările, salvate o singură dată; baza de date
  păstrează doar referința `artifact:<hash>`;
- `reports/` — rapoartele de milestone.

După `project add`, configurația proiectului din baza de date e sursa de adevăr; modificările
ulterioare ale fișierului JSON nu se aplică proiectului deja adăugat.

## Execuție reală (Faza 4)

În proiecte, Builder-ul nu mai scrie doar text: răspunsul lui devine **fișiere** pe branch-ul git
al taskului, platforma rulează **comanda de test stabilită de Owner**, iar munca intră în `main`
doar după trei porți.

```text
Builder răspunde cu fișiere  ──>  platforma le scrie pe branch-ul task/<id> și face commit
                                  └─> rulează test_command (ex. `cargo test`)
   poarta 1: fișiere valide + testele trec?   nu -> Builder primește rezultatul testelor (fără Reviewer)
   poarta 2: Reviewer-ul aprobă (vede dovada testelor, raportată de platformă)?
   poarta 3: git face merge în main fără conflict?   nu -> task FAILED, decizia Owner-ului
   apoi: push automat spre remote-ul Owner-ului (dacă e configurat)
```

**Comanda de test** se pune în JSON-ul proiectului sau ulterior:

```json
"test_command": "python -m unittest discover -s tests -v",
"test_timeout_secs": 120
```

```powershell
cargo run -- project configure demo-text-tools --test-command "cargo test" --test-timeout 300
cargo run -- project configure demo-text-tools --test-command ""      # fără teste
```

Agenții nu pot alege, schimba sau inventa comenzi. Fără `test_command`, fișierele sunt revizuite
așa cum au fost scrise.

Exemplu complet: `projects/demo-text-tools.json` (o mică bibliotecă Python testată cu `unittest`):

```powershell
cargo run -- project add projects\demo-text-tools.json
cargo run -- project plan demo-text-tools
cargo run -- project approve demo-text-tools
cargo run -- project run demo-text-tools
```

**Formatul răspunsului Builder-ului** (i se explică automat în prompt, împreună cu fișierele din
workspace):

````text
### FILE: texttools/core.py
```python
def word_count(text: str) -> int: ...
```
### DELETE: texttools/old_helpers.py
````

**Git** — fiecare proiect are un repository în `data/workspaces/<proiect>/`, creat gol la primul task:

- `main` — doar muncă aprobată, testată și integrată;
- `task/<id>` — branch-ul taskului, creat din `main`; **fiecare iterație a Builder-ului e un
  commit** cu autorul angajatului (ex. `EMP-BUILD-001`) și rezultatul testelor în mesaj;
- la aprobare, `EMP-ORCH-001` face `git merge --no-ff task/<id>` în `main`, cu mesaj
  „Merge <task>: <titlu> — Approved by EMP-REV-001 after N iteration(s)”; un conflict anulează
  merge-ul (`main` rămâne neatins) și oprește taskul;
- **un task reluat își aduce întâi branch-ul la zi cu `main`** (merge); dacă apar conflicte,
  fișierele rămân cu marcajele `<<<<<<<` și Builder-ul primește lista lor ca primă sarcină —
  testele și Reviewer-ul verifică rezolvarea, iar merge-ul final în `main` devine curat;
- un task oprit rămâne pe branch-ul lui — îl poți inspecta cu orice unealtă git
  (`git log main..task/<id>`, `git diff main task/<id>`);
- taskurile rulează pe rând, deci un singur working tree e suficient; între taskuri e pe `main`.

**Push automat** spre remote-ul ales de Owner (agenții nu îl pot schimba):

```powershell
cargo run -- project configure demo-text-tools --remote https://github.com/<cont>/<repo>.git
cargo run -- project configure demo-text-tools --remote ""     # doar commit-uri locale
```

După fiecare task se trimite branch-ul `task/<id>`, iar după fiecare merge și `main`. Niciodată cu
force; folosește autentificarea git a sistemului (Git Credential Manager) și nu așteaptă parole în
terminal. Un push eșuat nu schimbă rezultatul taskului — munca e în siguranță local, iar eroarea
apare la `project run` și în istoricul taskului.

**Siguranță:**

- căile din răspuns sunt validate strict: fără `..`, căi absolute, litere de unitate, `.git`,
  `target/`, `node_modules/` etc., nume rezervate Windows; maximum 60 de fișiere și 256 KB/fișier;
- testele rulează **local** (nu există încă un sandbox — Docker nu e instalat), cu directorul
  curent = workspace-ul taskului, timeout, și un mediu curățat: doar variabilele de sistem necesare
  (PATH, TEMP, USERPROFILE, CARGO_HOME…); cheile API și alte secrete nu ajung la teste;
- la timeout se oprește tot arborele de procese, nu doar shell-ul;
- platforma acționează în workspace doar pentru un Builder cu permisiunile `write_workspace` (și
  `run_tests` când există comandă de test); ele cer `write_implementation`, deci un Reviewer nu le
  poate avea — Reviewer-ul rămâne read-only;
- `task accept` (aprobarea manuală a Owner-ului) face și el merge doar fără conflict, cu mesajul
  „Accepted by the Owner (not approved by the Reviewer)”.

**Consum:** fiecare apel de model e contorizat (tokeni de intrare/ieșire, din răspunsul
serverului). `project status`, `project run` și `task show` arată totalurile; costul în USD apare
dacă pui prețurile în contract (`cost_per_mtok_input`, `cost_per_mtok_output` în blocul `model:`).

## Reutilizare la alt proiect

Nu modifici codul Rust: creezi `projects\proiect-nou.json` și rulezi `run --project` cu el.

## Subagenți, skill-uri și servere MCP (Faza 5a)

```text
EMP-BUILD-001 Builder (lead)      skills: python-unittest; mcp: pydoc
├─ EMP-PY-001 Python Developer   skills: python-unittest; mcp: pydoc
└─ EMP-QA-001 Testing Engineer   skills: python-unittest; mcp: pydoc
EMP-REV-001 Reviewer
└─ EMP-SEC-001 Security Specialist   skills: secure-code-review; VETO
```

**Delegare.** Un Builder cu `delegate_subtasks` și subagenți activi cu `write_implementation`
devine *lead*: la fiecare iterație împarte munca în subtaskuri (cel mult 4), fiecare cu un
subagent și **fișierele pe care le deține**. Control plane-ul verifică împărțirea (subagenți din
echipa lui, fișiere valide, niciun fișier în două subtaskuri); o împărțire invalidă e trimisă
înapoi o dată, apoi lead-ul lucrează singur. Fiecare subagent primește doar contextul subtaskului
lui (nu și celelalte subtaskuri); fișierele scrise în afara listei lui sunt **aruncate de platformă**.
Rezultatele se combină și trec prin aceleași porți (teste → Reviewer → merge). Auditul arată
`DELEGATION_PLANNED`, `SUBTASK_STARTED`/`SUBTASK_DONE` (cu ce s-a păstrat și ce s-a aruncat), cu
subagentul ca span copil al lead-ului.

**Consilierii Reviewer-ului și vetoul.** Subagenții Reviewer-ului cu `review_work` dau review-uri
consultative, incluse în promptul Reviewer-ului. Unul cu `veto_review` are drept de **veto**: dacă
respinge, rezultatul final e `CHANGES_REQUIRED`, orice ar decide Reviewer-ul (`VETO_APPLIED` în
audit). Un consilier cu veto care nu răspunde sau răspunde invalid contează ca respingere.

**Skill-uri** — pachete de instrucțiuni în `company/skills/<id>/SKILL.md`:

```markdown
---
name: Python with unittest
description: Small, standard-library Python modules with unittest tests.
---
- Tests live in `tests/` as `test_*.py` ...
```

Un angajat le primește prin `skill_packs: [python-unittest]` în contract; textul lor se adaugă la
fișa postului (promptul de sistem). Livrate: `python-unittest`, `secure-code-review`.

**Servere MCP** — definite o singură dată de Owner în `company/mcp.yaml`:

```yaml
servers:
  pydoc:
    description: Python standard-library documentation (local, read-only)
    command: python
    args: ["mcp/pydoc_server.py"]
    env: {}           # variabile literale, nesecrete
    env_from: []      # variabile copiate din mediul platformei (așa ajung secretele la un server)
    timeout_secs: 20
```

Un angajat le primește prin `mcp_servers: [pydoc]`. Platforma pornește serverul (transport stdio,
protocol MCP), îi citește uneltele și le oferă modelului prin tool calling în toate cele trei
dialecte (Ollama, OpenAI, Claude); modelul poate face cel mult 8 runde de apeluri per răspuns.
Fiecare apel apare în audit (`TOOL_CALL`: unealta, argumentele, dacă a fost eroare). Serverul
primește un mediu curățat plus doar ce permite `env`/`env_from`. Livrat: `pydoc`
(`company/mcp/pydoc_server.py`, doar biblioteca standard Python; refuză orice nu e în ea).

**Reguli** (validate la încărcarea registrului): un skill sau server inexistent face contractul
invalid; un subagent nu poate avea un server MCP pe care managerul lui nu-l are; `veto_review`
doar pentru un subagent al Reviewer-ului care are și `review_work`; intrările duplicate sunt
respinse. `doctor` pornește fiecare server MCP folosit și îi listează uneltele.

**Angajarea unui subagent** — aceeași procedură; scheletul are deja `skill_packs` și `mcp_servers`:

```powershell
cargo run -- hire propose EMP-PY-001 --name "Python Developer" --function specialist --manager EMP-BUILD-001
# completează TODO, skill_packs, mcp_servers în company\proposals\EMP-PY-001\
cargo run -- hire approve EMP-PY-001
```

Orchestratorul vorbește cu fiecare șef doar prin trait-ul `Agent` (`src/agents/mod.rs`), deci un
șef poate fi un singur apel LLM sau o echipă — orchestratorul nu vede diferența. Subagenții Python
prin Worker Gateway (Faza 5b) vor folosi același contract.
