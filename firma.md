# Firma AI - plan de constructie

## 1. Scop

Se construieste o firma software AI reutilizabila, in care **Owner-ul este singurul rol uman**, iar celelalte resurse de lucru sunt agenti AI si subagenti AI. Firma primeste lucrari sub forma de **proiecte**, iar Orchestratorul organizeaza executia acestora prin taskuri si subtasks.

Obiectivul arhitectural este ca echipa sa fie extensibila: sa poata fi adaugati ulterior noi angajati AI, noi specializari si noi proiecte fara modificarea nucleului de orchestrare.

## 2. Organigrama de baza

```text
                         OWNER (uman)
                              |
                              v
                  ORCHESTRATOR / Director
                              |
          +-------------------+-------------------+
          |                   |                   |
          v                   v                   v
      ARCHITECT             BUILDER             REVIEWER
    Sef arhitectura      Sef dezvoltare      Sef calitate
          |                   |                   |
          v                   v                   v
     Subagenti            Subagenti            Subagenti
     specialisti          executanti           verificatori
```

### Principiul de autoritate

- Owner-ul stabileste proiectele, prioritatile si aprobarile majore.
- Orchestratorul transforma proiectele in taskuri si le distribuie.
- Agentii principali coordoneaza domenii de responsabilitate.
- Subagentii executa lucrari specializate.
- Reviewer-ul este independent de Builder pentru controlul calitatii.

### Orchestratorul are doua componente

Orchestratorul nu este un singur LLM. Este impartit in:

```text
ORCHESTRATOR
  |
  +-- Control plane (Rust, determinist)
  |     stari, tranzitii, permisiuni, retry/timeout, audit, persistenta
  |
  +-- Planner (agent AI, EMP-PLAN-001)
        propune descompunerea proiectului in taskuri, dependente si echipa
```

- Planner-ul **propune**, control plane-ul **valideaza si aplica**.
- Un plan invalid (task fara criterii de acceptare, dependinta circulara, angajat inexistent sau
  fara skill-ul cerut) este respins de control plane, nu executat.
- Nicio tranzitie de stare nu este decisa de un model AI; modelul produce doar propuneri si
  artefacte, iar codul decide ce se intampla cu ele.

Fara aceasta separare, regula fundamentala (sectiunea 21) nu poate fi respectata.

## 3. Firma ca sistem de resurse umane digitale

Fiecare angajat AI are o identitate persistenta si o fisa completa. Angajatul nu este doar un prompt, ci o entitate administrata de platforma.

Campuri recomandate:

```text
employee_id
name
role
department
manager_id
employee_type        # agent | subagent
status               # active | suspended | disabled
contract_file
job_description_file
model_provider
model_name
tools
skills
permissions
forbidden_actions
budget_policy
kpi_policy
created_at
updated_at
```

## 4. Contractul operational digital

Contractul operational digital este echivalentul intern al contractului unui angajat, fara a reprezenta un contract individual de munca in sens juridic.

Trebuie sa defineasca:

- identitatea angajatului;
- functia ocupata;
- managerul direct;
- statutul;
- modelul AI autorizat;
- limitele de cost si resurse;
- programul/regimul de executie;
- instrumentele permise;
- drepturile de acces;
- actiunile interzise;
- regulile de confidentialitate si securitate;
- conditiile de suspendare;
- situatiile de escaladare la Orchestrator sau Owner.

Exemplu:

```yaml
employee_id: EMP-BUILD-001
name: Builder
role: Head of Engineering
manager_id: EMP-ORCH-001
employee_type: agent
status: active
model: opus
permissions:
  - read_project
  - write_workspace
  - run_tests
  - delegate_subtasks
forbidden_actions:
  - deploy_production
  - approve_own_work
  - read_secrets_without_grant
```

## 5. Fisa postului

Fiecare angajat AI are o fisa a postului separata de contractul operational.

Fisa postului trebuie sa contina:

1. scopul postului;
2. pozitia in organigrama;
3. cui raporteaza;
4. responsabilitati;
5. sarcini recurente;
6. competente si skill-uri;
7. tool-uri disponibile;
8. permisiuni;
9. actiuni interzise;
10. reguli de delegare;
11. livrabile asteptate;
12. KPI-uri;
13. conditii de escaladare.

## 6. Fisele celor patru roluri initiale

### 6.1 Owner

**Tip:** uman  
**Rol:** proprietar si autoritate finala.

Responsabilitati:

- introduce si aproba proiecte;
- stabileste obiective si prioritati;
- aproba actiunile cu risc major;
- stabileste bugete si limite;
- este singurul care poate aproba angajarea, suspendarea sau eliminarea angajatilor AI
  (Orchestratorul doar propune);
- decide asupra escaladarilor critice si raspunde la taskurile `HUMAN_REVIEW_REQUIRED`;
- aproba productia/deploy-ul daca politica o cere.

Owner-ul nu trebuie sa gestioneze fiecare task tehnic.

### 6.2 Orchestrator

**Employee ID recomandat:** `EMP-ORCH-001`  
**Functie:** Director operational / Project Director.

Responsabilitati, impartite pe cele doua componente (vezi sectiunea 2):

**Control plane (cod Rust, determinist):**

- primeste proiectele de la Owner;
- valideaza si aplica planul propus de Planner;
- atribuie taskuri agentilor;
- controleaza toate tranzitiile de stare;
- urmareste starea executiei;
- gestioneaza retry, timeout si blocaje;
- gestioneaza registrul de angajati;
- mentine auditul si persistenta;
- aplica politicile de acces si bugetele;
- escaladeaza la Owner cand este necesar.

**Planner (`EMP-PLAN-001`, agent AI):**

- analizeaza obiectivele proiectului;
- propune descompunerea in epics, taskuri si dependente;
- propune angajatii potriviti pentru fiecare task;
- propune angajari noi cand lipseste o competenta (aprobate de Owner).

Orchestratorul este autoritatea centrala asupra taskurilor; autoritatea apartine codului,
nu modelului AI.

### 6.3 Architect

**Employee ID recomandat:** `EMP-ARCH-001`  
**Functie:** Sef Departament Arhitectura.

Responsabilitati:

- transforma cerintele in specificatii tehnice;
- defineste module, contracte si interfete;
- stabileste dependentele;
- defineste criterii de acceptare;
- identifica riscuri de securitate si scalabilitate;
- solicita subagenti specialisti atunci cand este necesar;
- nu schimba obiectivul comercial primit de la Orchestrator.

### 6.4 Builder

**Employee ID recomandat:** `EMP-BUILD-001`  
**Functie:** Sef Departament Dezvoltare.

Responsabilitati:

- implementeaza specificatiile aprobate;
- coordoneaza subagentii executanti;
- integreaza rezultatele lor;
- ruleaza testele permise;
- gestioneaza modificarile in workspace;
- rezolva feedback-ul Reviewer-ului, pornind de la implementarea anterioara (corecteaza, nu
  rescrie de la zero);
- nu isi aproba singur munca;
- nu modifica arhitectura fara procedura de schimbare (sectiunea 12.1).

### 6.5 Reviewer

**Employee ID recomandat:** `EMP-REV-001`  
**Functie:** Sef Control Calitate.

Responsabilitati:

- verifica independent implementarea;
- compara implementarea cu specificatia Architectului;
- verifica testabilitatea, securitatea si mentenabilitatea;
- poate solicita subagenti de Security sau Performance;
- emite `APPROVED` sau `CHANGES_REQUIRED`, iar la `CHANGES_REQUIRED` indica cine corecteaza:
  `builder` (implementarea) sau `architect` (specificatia insasi);
- nu rescrie direct munca Builder-ului in fluxul normal.

Reguli de independenta:

- Reviewer-ul nu foloseste acelasi model ca Builder-ul pe acelasi task; daca politica permite
  totusi acelasi model, primeste context separat si nu vede rationamentul Builder-ului.
- Un raspuns al Reviewer-ului care nu respecta formatul structurat este tratat ca respingere,
  niciodata ca aprobare (fail-closed).

Agregarea verdictelor subagentilor de verificare:

- Security Specialist are drept de veto: un `CHANGES_REQUIRED` de securitate nu poate fi
  anulat de Reviewer.
- Ceilalti subagenti de verificare sunt consultativi; decizia finala apartine Reviewer-ului.

## 7. Subagentii - angajati specializati

Subagentii sunt angajati AI executivi/specialisti. Ei pot avea acelasi nivel tehnic sau chiar unul foarte ridicat, dar autoritatea lor operationala este limitata la sarcina delegata.

Exemple:

```text
Voice Specialist
Asterisk/SIP Specialist
Python Developer
Rust Developer
Database Specialist
Testing Engineer
Security Specialist
Performance Engineer
RAG Specialist
IoT/MQTT Specialist
Django Specialist
Next.js Specialist
```

Un subagent trebuie sa primeasca doar contextul necesar sarcinii sale. Aceasta regula este
impusa de platforma: agentul principal nu transmite mai departe ordinul de lucru primit, ci
construieste pentru fiecare subagent un ordin restrans (subtask, partea relevanta din
specificatie, criteriile de acceptare aplicabile).

Reguli de delegare:

- adancimea maxima de delegare este **un nivel**: Agent -> Subagent; un subagent nu poate
  delega mai departe;
- un subagent nu poate primi permisiuni mai largi decat agentul care l-a delegat;
- fiecare subtask are propriul `task_id` si propriul span de audit, legat de taskul parinte.

## 8. Registrul de angajati

Platforma trebuie sa contina un registru extensibil:

```text
company/
  employees/
    EMP-ORCH-001/
      contract.yaml
      job_description.md
    EMP-PLAN-001/
      contract.yaml
      job_description.md
    EMP-ARCH-001/
      contract.yaml
      job_description.md
    EMP-BUILD-001/
      contract.yaml
      job_description.md
    EMP-REV-001/
      contract.yaml
      job_description.md
    EMP-PY-001/
      contract.yaml
      job_description.md
```

Registrul permite adaugarea unui angajat fara recompilarea nucleului.

## 9. Procedura de angajare a unui nou angajat AI

### Pasul 1 - definirea postului

Orchestratorul (prin Planner) sau Owner-ul identifica necesitatea unui nou rol. Orchestratorul
poate doar **propune** angajarea; angajarea efectiva necesita aprobarea Owner-ului.

Exemplu:

```text
Necesitate: proiectele necesita dezvoltare Python frecventa.
Post nou: Python Developer.
```

### Pasul 2 - creare Employee ID

```text
EMP-PY-001
```

### Pasul 3 - contract operational

Se definesc modelul, managerul, permisiunile, tool-urile, bugetul si restrictiile.

### Pasul 4 - fisa postului

Se definesc responsabilitatile, skill-urile, KPI-urile si livrabilele.

### Pasul 5 - aprobare si inregistrare

Dupa aprobarea Owner-ului, angajatul este adaugat in registrul firmei cu status `active`.
Control plane-ul valideaza contractul la incarcare (manager existent, model autorizat,
permisiuni cunoscute, permisiuni nu mai largi decat ale managerului).

### Pasul 6 - disponibilitate pentru Orchestrator

Orchestratorul il poate selecta la taskurile compatibile cu skill-urile sale.

## 10. Proiectele sunt lucrarile firmei

Firma primeste lucrari sub forma de proiecte.

Structura minima:

```text
project_id
name
objective
priority
status
requirements
acceptance_criteria
assigned_team
created_at
```

Exemplu:

```text
PROJECT: VOICE-001
Name: Voice Agent Core
Objective: nucleu vocal modular si scalabil
Priority: High
Status: IN_PROGRESS
```

## 11. Orchestratorul creeaza si distribuie taskurile

Owner-ul nu distribuie taskurile tehnice de rutina.

Flux:

```text
Owner
  |
  v
Project
  |
  v
Orchestrator
  |
  +--> TASK-001 -> Architect
  +--> TASK-002 -> Builder
  +--> TASK-003 -> Reviewer
```

Agentul poate cere subtasks pentru subagenti:

```text
TASK-002 -> Builder
               |
               +--> SUBTASK-002A -> Python Developer
               +--> SUBTASK-002B -> Rust Developer
               +--> SUBTASK-002C -> Testing Engineer
```

Orice subtask trebuie sa ramana trasabil in proiect.

## 12. Fluxul standard al unui proiect

```text
1. Owner creeaza / aproba proiectul
2. Planner analizeaza proiectul si propune planul
3. Control plane valideaza planul si creeaza taskurile
4. Architect produce specificatia
5. Builder implementeaza
6. Subagentii executa lucrari specializate
7. Reviewer verifica independent
8. Daca CHANGES_REQUIRED (target builder) -> taskul revine la Builder
   Daca CHANGES_REQUIRED (target architect) -> procedura de schimbare (12.1)
9. Daca APPROVED -> taskul este inchis
10. Daca se atinge limita de iteratii -> HUMAN_REVIEW_REQUIRED (sectiunea 13.1)
11. Orchestrator actualizeaza progresul proiectului
12. La milestone/finalizare -> raport catre Owner
```

### 12.1 Procedura de schimbare a arhitecturii

Builder-ul nu modifica singur specificatia. Cand Reviewer-ul constata ca specificatia insasi
este gresita sau incompleta (Builder-ul nu poate indeplini criteriile urmand-o):

1. Reviewer-ul emite `CHANGES_REQUIRED` cu `target: architect` si motivul.
2. Control plane-ul trimite specificatia curenta, ultima implementare si feedback-ul la Architect.
3. Architect-ul produce o revizie noua a specificatiei (numar de revizie incrementat).
4. Builder-ul continua pe noua revizie, pornind de la implementarea anterioara.

Limite:

- numarul de revizii de arhitectura per task este limitat (`max_architecture_revisions`,
  implicit 1);
- peste limita, feedback-ul merge la Builder, iar daca taskul nu se inchide in limita de
  iteratii, devine `HUMAN_REVIEW_REQUIRED`;
- o schimbare care afecteaza obiectivul proiectului sau contracte publice deja aprobate
  necesita aprobarea Owner-ului.

## 13. Starile recomandate pentru taskuri

```text
NEW
PLANNED
ASSIGNED
IN_PROGRESS
BLOCKED
REVIEW
CHANGES_REQUIRED
APPROVED
DONE
HUMAN_REVIEW_REQUIRED
FAILED
CANCELLED
```

Tranzitiile sunt controlate de Orchestrator, nu lasate liber modelului AI.

Diferenta dintre cele doua stari de oprire:

- `HUMAN_REVIEW_REQUIRED` - munca nu a trecut de review in limita de iteratii; este nevoie de o
  decizie a Owner-ului.
- `FAILED` - eroare tehnica (model indisponibil dupa retry, timeout, raspuns care incalca
  protocolul); motivul este inregistrat in audit, iar taskul poate fi reluat dupa remediere.

### 13.1 Escaladarea si raspunsul Owner-ului

Un task in `HUMAN_REVIEW_REQUIRED` sau `BLOCKED` asteapta, persistat, decizia Owner-ului.
Owner-ul poate:

- **relua** taskul cu instructiuni suplimentare (iteratiile se reseteaza sau se suplimenteaza);
- **accepta** rezultatul existent cu observatii (aprobare manuala, marcata ca atare in audit);
- **modifica** obiectivul sau criteriile de acceptare;
- **anula** taskul (`CANCELLED`).

Fiecare decizie a Owner-ului este o actiune auditata ca oricare alta.

## 14. Structura tehnica recomandata

```text
ai-company/
  control-plane/          # Rust
    orchestrator/
    employee_registry/
    project_manager/
    task_manager/
    permissions/
    audit/
    model_gateway/

  company/
    employees/
    departments/
    policies/

  projects/
    PROJECT-001/
    PROJECT-002/

  workers/                # subagenti
    python/
    rust/

  artifacts/              # continut adresat prin hash
  audit/
  data/                   # baza de date (proiecte, taskuri, stari)
```

### 14.1 Persistenta

Proiectele, taskurile, starile lor si deciziile Owner-ului se stocheaza persistent (initial
SQLite, local). Fara persistenta:

- o rulare se pierde daca procesul se opreste;
- un task nu poate astepta raspunsul Owner-ului (`BLOCKED`, `HUMAN_REVIEW_REQUIRED`) si apoi
  continua;
- proiectele simultane si progresul pe milestone-uri nu sunt posibile.

Artefactele (specificatii, implementari, rapoarte) se salveaza separat, adresate prin hash;
baza de date si auditul pastreaza doar referintele.

## 15. Separarea responsabilitatilor tehnice

### Rust - conducerea firmei

- Orchestrator;
- agent controllers;
- registru de angajati;
- project manager;
- task manager;
- permisiuni;
- workflow;
- retry/timeouts;
- audit;
- model gateway;
- worker gateway.

### Python - specialistii AI

- ML;
- RAG;
- Voice/STT/TTS;
- document processing;
- data analysis;
- specialisti framework;
- automatizari;
- test generation.

## 16. Comunicarea dintre angajati

Comunicarea trebuie sa fie structurata:

```json
{
  "project_id": "VOICE-001",
  "task_id": "TASK-014",
  "sender": "EMP-REV-001",
  "receiver": "EMP-BUILD-001",
  "type": "CHANGES_REQUIRED",
  "message": "Adauga timeout handling si teste de reconnect."
}
```

Nu se recomanda conversatii libere si necontrolate intre toti agentii.

## 17. Permisiuni pe roluri

Permisiunile sunt **capabilitati de unelte**, nu descrieri in prompt. Un angajat poate face
doar ce ii permit uneltele primite de la platforma; o permisiune care nu corespunde unei unelte
nu are efect si nu trebuie definita.

Exemple de capabilitati:

```text
read_project          -> citire documentatie si cerinte
read_workspace        -> citire fisiere din workspace-ul taskului
write_workspace       -> scriere doar in workspace-ul alocat taskului
run_tests             -> rulare test runner in workspace
delegate_subtasks     -> creare subtasks pentru subagenti permisi
read_secret:<nume>    -> acces la un secret anume, doar cu grant
```

Secretele nu sunt niciodata incluse in prompt sau in artefacte. Accesul se face printr-un
**grant** explicit (angajat, secret, task, durata), aprobat conform politicii si auditat;
grantul expira la inchiderea taskului.

### Orchestrator

Poate:
- crea si distribui taskuri;
- activa agenti;
- consulta registrul;
- opri un run;
- escalada.

Nu trebuie sa:
- ocoleasca politicile Owner-ului;
- permita acces necontrolat la secrete.

### Architect

Poate:
- citi cerintele si documentatia;
- crea specificatii;
- solicita specialisti.

Nu poate:
- deploy in productie;
- aproba implementarea finala.

### Builder

Poate:
- modifica workspace-ul alocat;
- rula build/test;
- delega subtasks permise.

Nu poate:
- aproba propria munca;
- modifica productie fara aprobare.

### Reviewer

Poate:
- citi cod, diff-uri si teste;
- cere modificari;
- aproba conform politicii.

Nu poate:
- modifica direct implementarea in fluxul standard.

## 18. KPI pentru angajatii AI

Exemple:

```text
Task completion rate
Review pass rate
Average correction cycles
Defects after approval
Policy violations
Average task duration
Cost per completed task
Tool failure rate
Escalation rate
```

KPI-urile nu trebuie folosite pentru a incuraja aprobari superficiale; calitatea si siguranta sunt guardrails.

Conditii pentru a putea masura:

- model gateway-ul inregistreaza pentru fiecare apel: angajat, task, model, tokeni, cost,
  durata, rezultat; fara aceasta contorizare, `budget_policy`, costul per task si KPI-urile nu
  pot fi calculate;
- "Defects after approval" necesita un flux de raportare dupa aprobare (Owner-ul sau un test
  ulterior marcheaza un defect pe un task inchis, legat de aprobarea respectiva).

## 19. Audit

Fiecare actiune importanta trebuie inregistrata:

```text
run_id
project_id
task_id
parent_task_id        # pentru subtasks
span_id
parent_span_id        # cine a delegat actiunea
employee_id
action
input_reference       # hash-ul artefactului de intrare
output_reference      # hash-ul artefactului produs
model
tools_used
tokens / cost
timestamp
result
```

Auditul este append-only si pastreaza **referinte** catre artefacte, nu continutul lor
integral la fiecare eveniment. Span-urile permit reconstituirea arborelui
Orchestrator -> Agent -> Subagent pentru orice rezultat.

Trebuie sa putem raspunde ulterior la intrebarea:

> Cine a facut aceasta modificare, pentru ce task, pe baza carei specificatii si cine a aprobat-o?

## 20. Etapele de constructie

Ordinea fazelor este aleasa astfel incat permisiunile sa devina efective cat mai devreme:
uneltele asupra carora se aplica permisiunile (faza 4) vin inaintea subagentilor Python
(faza 5). Altfel, intre definirea permisiunilor si aparitia uneltelor, permisiunile ar fi doar
declarative.

### Faza 1 - conducerea (REALIZATA)

- Owner conceptual;
- Orchestrator Rust;
- Architect;
- Builder;
- Reviewer;
- workflow si audit.

Realizat in `ai-team-rust` v0.1:

- workflow controlat de cod, cu limita de iteratii si `HUMAN_REVIEW_REQUIRED`;
- Reviewer fail-closed si procedura de schimbare a arhitecturii (`target: architect`);
- Builder care corecteaza implementarea anterioara;
- retry/timeout pe apelurile de model, `FAILED` inregistrat in audit;
- contract comun `Agent` prin care un agent principal poate delega catre subagenti;
- audit JSONL cu span-uri parinte/copil.

### Faza 2 - registrul de angajati (REALIZATA)

Realizat in `ai-team-rust`:

- registru in `company/employees/<ID>/` (`contract.yaml` + `job_description.md`), validat la
  fiecare incarcare; fisa postului este promptul de sistem al agentului;
- `config/team.json` eliminat - echipa vine din registru;
- permisiuni ca actiuni impuse de platforma: `propose_plan`, `write_specification`,
  `write_implementation`, `review_work`, `delegate_subtasks`;
- reguli impuse: un singur orchestrator, ierarhie Owner -> Orchestrator -> Agent -> Subagent,
  permisiuni subagent incluse in ale managerului, adancime de delegare 1, separarea
  `write_implementation` / `review_work`;
- Planner (`EMP-PLAN-001`) propune echipa per rulare; control plane-ul o valideaza
  (`PLAN_REJECTED` la plan invalid); `assigned_team` din proiect (decizia Owner-ului) are prioritate;
- avertisment de politica (`POLICY_WARNING`) cand Builder si Reviewer folosesc acelasi model;
- `hire propose` / `hire check` / `hire approve` si `employees set-status`, auditate in
  `.ai-team/hr.jsonl`.

Ramas pentru fazele urmatoare: `budget_policy` / `kpi_policy` (necesita contorizare, faza 4),
`created_at` / `updated_at` in contract (momentul angajarii este deocamdata in auditul HR),
propunerea automata de angajari de catre Planner.

Continut planificat initial:

- `Employee` generic;
- contract operational si fisa postului, citite din `company/employees/` (inlocuiesc
  configuratia statica a echipei);
- status;
- manager;
- skill-uri;
- permisiuni definite ca capabilitati de unelte (sectiunea 17);
- validarea contractelor la incarcare;
- comanda `hire` (propunere + aprobare Owner);
- separarea Orchestrator = control plane + Planner.

### Faza 3 - proiecte si taskuri (REALIZATA)

Realizat in `ai-team-rust`:

- persistenta SQLite (`data/ai-team.db`): proiecte, planuri, milestone-uri, taskuri, dependente,
  rulari, decizii ale Owner-ului; depozit de artefacte adresat prin hash (`data/artifacts/`) -
  baza de date pastreaza doar referintele;
- Planner-ul propune planul de taskuri (milestone-uri, taskuri, criterii, dependente); control
  plane-ul il valideaza (fara cicluri, dependente doar spre acelasi milestone sau unul anterior,
  maximum 25 de taskuri) si il trimite inapoi cu motivul daca e invalid; planul ruleaza doar dupa
  aprobarea Owner-ului (`project approve`);
- executie secventiala: urmatorul task cu dependentele `DONE`; taskurile intrerupte se reiau din
  starea salvata (echipa, specificatia si ultima implementare se pastreaza);
- starile din sectiunea 13, inclusiv `BLOCKED` calculat automat din dependente;
- deciziile Owner-ului din sectiunea 13.1: `task resume` (cu nota si iteratii suplimentare),
  `task accept` (aprobare manuala, raportata distinct), `task cancel`;
- raport catre Owner la inchiderea fiecarui milestone (`data/reports/`).

Ramas pentru fazele urmatoare: executie paralela si proiecte simultane (faza 6); auditul JSONL al
rularilor contine inca starea completa, nu doar referinte (de aliniat cu depozitul de artefacte);
modificarea planului dupa aprobare (acum un proiect are un singur plan aprobat; taskurile se pot
doar relua, accepta sau anula).

Continut planificat initial:


- persistenta (SQLite) pentru proiecte, taskuri, stari si deciziile Owner-ului;
- Project Manager;
- Task Manager;
- assignment;
- dependencies;
- status;
- milestones;
- reluarea taskurilor dupa escaladare (sectiunea 13.1).

### Faza 4 - executie reala (minima) (REALIZATA)

Realizat in `ai-team-rust`:

- Git branch per task: fiecare proiect are un repository (`data/workspaces/<proiect>`), creat gol;
  fiecare task lucreaza pe `task/<id>`, fiecare iteratie a Builder-ului e un commit cu autorul
  angajatului, iar munca aprobata intra in `main` printr-un merge git (`--no-ff`); push automat
  (fara force) spre remote-ul ales de Owner;
- Builder-ul raspunde cu fisiere (`### FILE: cale` + bloc de cod) si stergeri (`### DELETE: cale`),
  validate strict (fara `..`, cai absolute, `.git`, directoare de build, nume rezervate; limite);
- test runner: comanda de test e a Owner-ului (`test_command`), agentii nu o pot alege; ruleaza in
  workspace-ul taskului, cu timeout (se opreste tot arborele de procese) si fara secrete in mediu;
- approval gates: (1) fisiere valide si teste trecute - altfel Builder-ul primeste rezultatul
  testelor si munca nu ajunge la Reviewer; (2) aprobarea Reviewer-ului, care vede dovada testelor
  raportata de platforma; (3) merge git in `main` doar fara conflict;
- reviewer read-only (nu poate avea `write_workspace`/`run_tests`, care cer `write_implementation`);
- permisiuni noi efectiv aplicate: `write_workspace`, `run_tests`;
- contorizare tokeni (si cost, cu preturi optionale in contract) per apel, per task si per proiect;
- depozit de artefacte adresat prin hash (din faza 3).

Ramas: sandbox real pentru teste (Docker nu e instalat - acum ruleaza local, cu restrictii);
pornirea unui proiect dintr-un repository existent; executie paralela (ar cere `git worktree`).

Continut planificat initial:


- workspace per task;
- Git branch per task;
- filesystem tools, controlate de permisiuni;
- test runner;
- depozit de artefacte adresat prin hash;
- reviewer read-only;
- approval gates;
- contorizare tokeni/cost in model gateway.

### Faza 5 - subagenti

- Worker Gateway;
- protocol Rust-Python;
- generic Python Worker;
- registru de subagenti;
- delegare controlata (un nivel, context restrans, permisiuni mostenite restrictiv);
- verdictul Security cu drept de veto in review.

### Faza 6 - extindere organica

- angajare de noi specialisti;
- departamente suplimentare;
- proiecte simultane;
- scheduling;
- bugete;
- dashboard.

## 21. Regula fundamentala

Arhitectura firmei trebuie sa ramana:

```text
Owner
  -> Project
  -> Orchestrator (control plane + Planner)
  -> Task
  -> Agent
  -> Subtask
  -> Subagent
  -> Result
  -> Reviewer
  -> Approval
```

Agentii si subagentii sunt resurse digitale ale firmei, dar autoritatea, permisiunile si traseul muncii sunt impuse de platforma, nu doar descrise in prompt.

## 22. Rezultatul dorit

La final, firma trebuie sa permita:

- primirea unui proiect nou;
- planificarea automata a lucrarii;
- formarea echipei potrivite;
- atribuirea taskurilor;
- delegarea catre specialisti;
- executia si verificarea muncii;
- raportarea progresului;
- audit complet;
- adaugarea de noi angajati AI fara modificarea nucleului;
- reutilizarea aceleiasi echipe pentru proiecte diferite.

Aceasta este baza unei **firme software AI reutilizabile**, nu doar a unui singur agent sau a unui workflow izolat.
