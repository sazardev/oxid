# MULTINODE.md — Plan para varios nodos

> Plan de ejecución para pasar Oxid de un servidor a una flota. Escrito en
> español como `ROADMAP.md`, que es el otro documento de planificación del
> repositorio. Todo lo que aquí se afirma sobre el código actual fue
> verificado leyéndolo o compilándolo, no recordado; donde algo está sin
> comprobar, lo dice.

---

## 1. Por qué, y por qué no antes

Oxid es hoy **un daemon, un servidor**. Cuando se acaba el hierro, se acabó:
no hay forma de añadir una segunda máquina.

Conviene decir de entrada que esto **no es una omisión, es una frontera de
alcance deliberada**. `SPEC.md` habla de «métricas globales del **nodo**», en
singular; `IDEA.md` y `DESIGN.md` nunca mencionan varios servidores. Un
control plane de un nodo gobernando un plano de datos de varios nodos es un
producto coherente. Media alta disponibilidad no lo es.

Lo que sí cambió es la evidencia. Probando el bundle publicado en producción
aparecieron dos fallos que *son* de concurrencia y que ya muerden con un solo
servidor:

- la cola de deploys sólo la protegía un mutex en proceso, y su propio
  comentario admitía que dos drenajes desplegarían el mismo push dos veces;
- dos ramas podían recibir el mismo índice de Redis, porque el índice único
  de `resource_leases` es por rama y no por slot.

Ambos están **ya corregidos** (commits `62e3d00` y `5dd5113`). Eso importa
para este plan por dos razones: la etapa 0 de multi-nodo ya está entregada, y
demuestra que «dos procesos sobre un mismo directorio de datos» no es
hipotético — es lo que ocurre unos segundos cada vez que se reinicia un
contenedor.

**Resultado buscado:** un operador registra un segundo servidor con
`oxid node add`, y a partir de ahí los entornos se reparten entre ambos sin
que ninguna otra cosa cambie.

---

## 2. Inventario: qué asume exactamente un solo nodo

Verificado fichero a fichero. Esta tabla es el trabajo real; el resto del
documento es consecuencia suya.

| Pieza | Qué asume | Dónde |
|---|---|---|
| Estado | SQLite local en WAL; las migraciones las corre quien abra el fichero | `adapter/store.rs` |
| Directorios | `git-cache/`, `secret.key`, `backups/`, `.restore-pending.tar` en disco local | `main.rs` |
| Clave maestra | `ArcSwap<Cipher>` en proceso: un segundo daemon no vería la rotación | `store.rs::rotate_master_key` |
| Bloqueos | `KeyedLocks` en memoria: rama, caché git, admisión, slots de pool | `service/keyed_lock.rs` |
| Coalescedor de fetch | Clave `Instant`, que es local al proceso | `service/refresh_coalescer.rs` |
| Planificador | Barre **todos** los entornos y drena la cola; sin elección de líder | `service/scheduler.rs` |
| Reconciliación | Contrasta cada fila contra el Docker **local** al arrancar | `gc.rs::reconcile_startup_state` |
| Admisión | `docker info` local, y suma **todas** las filas sin columna de nodo | `admission.rs` |
| Proxy | Registro en memoria; upstream fijo a `127.0.0.1` | `service/proxy.rs:145` |
| Nombres | `oxid-{proyecto}-{rama}-{id}` es único por *host*, no por flota | `helpers.rs` |
| `Environment` | **No tiene campo de nodo** en ninguna de las 19 migraciones | `domain/environment.rs` |
| Traefik | Se arranca en local y descubre contenedores por etiquetas del socket local | `adapter/oci.rs`, `infra.rs` |

Una consecuencia importante y contraintuitiva: **la mayoría de esos bloqueos
siguen siendo correctos** mientras haya un solo *control plane*, aunque haya
muchos nodos. Eso es exactamente lo que compra la opción recomendada, y es el
argumento a esgrimir cuando alguien proponga un store distribuido.

---

## 3. La decisión de arquitectura

### Recomendación: un control plane, N endpoints Docker remotos sobre mTLS

```
                          oxid-cli · webhooks · panel
                                     │
   ┌───────────────────── nodo del control plane ─────────────────────┐
   │  Traefik ──(proveedor HTTP)──> oxidd /api/v1/traefik/config      │
   │     │                              │                            │
   │     └──> ProxyRegistry (public_port estable por rama)            │
   │                    audit.sqlite · git-cache · secret.key         │
   └────────────────────┬─────────────────────────────────────────────┘
                        │  TCP a node.address:host_port
        ┌───────────────┴────────┐   ┌────────────────────────┐
        │ nodo "eu-1"            │   │ nodo "eu-2"            │
        │ dockerd tcp/2376 mTLS  │   │ dockerd                │
        │ oxid-app-feat-x-41 …   │   │ …                      │
        └────────────────────────┘   └────────────────────────┘
```

**Tres hechos la sostienen, los tres verificados:**

1. **`ContainerPort` no cambia.** Sus 22 métodos ya son agnósticos del nodo:
   toman un nombre de contenedor, una especificación, una etiqueta de imagen.
   Multi-nodo es un cambio de **cardinalidad** (`oci: O` → un mapa de `O`),
   no de contrato. La costura hexagonal ya está pagada.

2. **Cuesta cero dependencias.** `bollard::Docker::connect_with_ssl` existe
   en la 0.17.1 que ya usamos, y su feature `ssl` la arrastra `buildkit`, que
   ya está activada. `rustls 0.23.43` y `hyper-rustls 0.27.9` ya están en el
   `Cargo.lock`. **Comprobado compilando** una llamada de prueba contra el
   workspace tal cual está: pasa `cargo check` sin tocar nada. Para un
   proyecto cuyo valor declarado es «superficie de dependencias pequeña, un
   binario», eso no es un desempate: es la respuesta.

3. **Los builds se quedan donde están.** `tar_context` ya envía el contexto
   al endpoint de build, así que el build corre **en el nodo** mientras la
   caché de git sigue en el control plane. `LockKey::GitCache` sigue siendo
   correcto sin coordinación distribuida alguna. Ésta es la razón principal
   por la que un endpoint remoto gana a un agente propio.

### Por qué no un agente por nodo

Un agente significa reimplementar la API remota de Docker: 22 métodos sobre
HTTP, incluido un endpoint de build que transmite un tar de cientos de
megabytes y un relevo de `stream_logs`, más un contrato de compatibilidad de
versiones entre control plane y agente que ha de sobrevivir a
actualizaciones escalonadas. Semanas de protocolo para obtener algo que
bollard ya da gratis.

Tiene **dos ventajas reales**, y ninguna urge todavía:

- **Confianza.** Un socket Docker sobre TCP equivale a root en el nodo. mTLS
  acota *quién* llega, no *qué* puede hacer. Un agente podría imponer «sólo
  contenedores que se llamen `oxid-*`». Si algún día Oxid corre en nodos que
  el operador no controla del todo, ésa es la razón para construirlo.
- **El camino de datos.** Un agente podría correr el proxy por rama en local,
  sacando al control plane del camino de cada petición (ver §5).

El plan **deja esa puerta abierta por un coste único**: un
`adapter/agent.rs` que implemente `ContainerPort` junto a `oci.rs`, **sin
tocar `ControlPlane`**. Conviene dejarlo escrito en el comentario del módulo
de flota para que la opción siga siendo visible.

### Por qué no un control plane en alta disponibilidad

Con franqueza: SQLite aquí no es un detalle de implementación, es una
decisión de producto **con mediciones detrás**. `CLAUDE.md` registra el
rendimiento del heartbeat pasando de ~180 req/s a 948–5108 req/s gracias a
WAL más `idx_environments_url`. `adapter/store.rs` son 2.953 líneas de
comportamiento específico de SQLite: la regla de una sola conexión en
`open_in_memory`, el `BEGIN IMMEDIATE` de `rotate_master_key`, los backups
por `VACUUM INTO`, el truco de unicidad parcial con `COALESCE(project_id,-1)`
de la migración `0001`. Cambiarlo por Postgres o por un Raft embebido
canjea todo eso por una dependencia externa que el operador tendría que
poner en alta disponibilidad **de todos modos**.

Mejor decir el modo de fallo con claridad que fingir que no existe:

> **Si el control plane cae, los entornos que están corriendo siguen
> sirviendo.** Los contenedores llevan política de reinicio y, bajo el
> enrutado de §5, el proxy frontal es un contenedor aparte. Lo que se
> detiene es: desplegar, despertar, la GC y la API. La recuperación es
> «restaurar `{data}/backups/` y reiniciar», maquinaria que ya existe en
> `service/backup.rs`.

---

## 4. Esquema

### `0020_nodes.sql`

```sql
CREATE TABLE nodes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    -- 'local' es el socket que este daemon ya usa. Cualquier otra cosa es
    -- un endpoint bollard (`tcp://host:2376`).
    endpoint TEXT NOT NULL,
    -- Dónde marca el proxy del control plane los puertos publicados de este
    -- nodo. Distinto de `endpoint`: la API de Docker y el tráfico de los
    -- contenedores pueden vivir legítimamente en interfaces distintas.
    address TEXT,
    tls_ca_path TEXT, tls_cert_path TEXT, tls_key_path TEXT,
    -- active | draining | down. `draining` rechaza colocaciones nuevas pero
    -- sigue sirviendo; `down` lo pone el sondeo de salud y NUNCA se propaga
    -- a los entornos que hay en él.
    state TEXT NOT NULL DEFAULT 'active',
    reserved_memory_mb INTEGER,
    total_memory_bytes INTEGER NOT NULL DEFAULT 0,
    cpu_count INTEGER NOT NULL DEFAULT 0,
    last_seen_at INTEGER,
    created_at INTEGER NOT NULL
);

-- Toda instalación existente es un nodo, y es éste. Sembrarlo aquí y no al
-- arrancar es lo que permite rellenar `environments.node_id` en la misma
-- migración, para que ninguna fila quede jamás sin nodo.
INSERT INTO nodes (id, name, endpoint, state, created_at)
VALUES (1, 'local', 'local', 'active', unixepoch());

-- SQLite prohíbe ADD COLUMN con REFERENCES y un DEFAULT no nulo a la vez,
-- así que entra nullable y se rellena de inmediato. Un NULL leído después
-- significa «escrito por un binario anterior a esta migración» y se resuelve
-- como nodo 1 — la misma respuesta, y que además sobrevive a volver atrás.
ALTER TABLE environments ADD COLUMN node_id INTEGER REFERENCES nodes(id);
UPDATE environments SET node_id = 1 WHERE node_id IS NULL;
CREATE INDEX IF NOT EXISTS idx_environments_node ON environments(node_id);
```

`resource_leases` **no** gana columna de nodo a propósito: un Postgres o un
Redis compartidos son servicios de toda la flota, alcanzados por URL, no
recursos locales de un nodo. `projects`, `secrets`, `audit_events`,
`api_tokens`, `deploy_queue`, `pull_requests` y `forge_notifications` quedan
igual.

### Dominio

`crates/oxid-core/src/domain/node.rs` (nuevo): `NodeId(pub u64)`,
`NodeState { Active, Draining, Down }`, `Node { id, name, endpoint, address,
state, capacity: HostCapacity, reserved_memory_mb, last_seen_at }`. Puro,
`serde`, sin dependencias nuevas.

`Environment` gana `pub node_id: NodeId`, y **`Environment::new` lo
inicializa a `NodeId(1)`**. Ese detalle es lo que mantiene compilando —y
significando lo correcto— todos los sitios que lo construyen, incluidas las
2.739 líneas de `service/control_plane/tests.rs`.

En `store.rs`: `ENV_COLUMNS`/`ENV_COLUMNS_NO_ID` ganan `node_id`, y los
mapeadores lo traducen con `None → NodeId(1)`.

---

## 5. Enrutado

### Recomendación: un Traefik en el control plane, alimentado por el proveedor HTTP del daemon

Para un entorno que vive en un nodo remoto:

1. Los nodos corren en **modo direct-publish** (`ContainerSpec.network =
   None`), que ya existe, ya está probado y ya guarda en
   `Environment.host_port` el puerto que Docker eligió.
2. El `ProxyRegistry` del control plane ata el `public_port` estable de la
   rama como hoy, pero marca `node.address:host_port` en vez de
   `127.0.0.1:host_port`. Son dos líneas: `proxy.rs::spawn_accept_loop` y
   `wait_until_ready` pasan a tomar host además de puerto.
3. Traefik deja de descubrir contenedores por etiquetas y lee configuración
   dinámica del daemon:
   `--providers.http.endpoint=http://oxid-daemon:8080/api/v1/traefik/config`,
   autenticada con `--providers.http.headers.Authorization`. Cada entorno se
   convierte en un router `Host(url)` hacia un servicio
   `http://127.0.0.1:{public_port}`, con los mismos middlewares de heartbeat
   (`forwardauth`) y de despertar (`errors`) que hoy construye
   `infra.rs::traefik_labels`.

Esa configuración la genera una **función pura en `oxid-core`** —
`domain/services/routing.rs::dynamic_config(...) -> DynamicConfig` — que
devuelve structs `serde` planos. Sin dependencias nuevas (serde ya está),
enteramente testeable, y hace que `traefik_labels` y el router de flota
compartan **un solo conjunto de reglas probado** en vez de derivar por
separado.

### Por qué no las alternativas

- **Red overlay de Swarm.** Los contenedores serían alcanzables, pero el
  proveedor Docker de Traefik sólo ve el socket local, así que nunca se
  enteraría de los contenedores remotos. Acabarías construyendo el proveedor
  HTTP igualmente, más una dependencia de Swarm. Descartado.
- **Un Traefik por nodo con DNS.** Es real, y es la respuesta correcta a una
  escala bastante mayor. Pero necesita DNS comodín por nodo, arranque de red
  por nodo, un catch-all por nodo — y hoy `oxid-wake-catchall` vive en el
  contenedor *del propio daemon*, que sólo existe en el nodo del control
  plane — y un `daemon_url` alcanzable desde cada nodo. Queda como la vía de
  escape documentada para cuando el ancho de banda del control plane sea el
  techo, no como v1.
- **Traefik apuntando directo a `node.address:host_port`.** Tentador, pero
  `host_port` cambia en cada redespliegue y el proveedor HTTP de Traefik
  sondea (5 s por defecto), así que cada redespliegue reintroduciría una
  ventana sin ruta — deshaciendo justo lo que la migración `0007` consiguió.
  Mantener el proxy preserva el corte atómico.

### Despertar y heartbeat: preservados, y de hecho mejorados

`find_by_url` resolviendo sólo por URL sigue siendo correcto: una URL es
única en la flota por la misma razón que hoy lo es en el nodo
(`deploy_at` rechaza un subdominio en colisión). `wake_by_url` pasa a
despachar por `env.node_id`. Nada del contrato de despertar cambia.

Y hay una mejora que conviene aprovechar: hoy un contenedor parado no tiene
router propio, y por eso existe el frágil `oxid-wake-catchall` de prioridad
mínima en el contenedor del daemon (documentado como crítico en `CLAUDE.md`,
y que `oxid infra status` reporta si falta). Bajo el proveedor HTTP el router
se genera **desde la fila de la base de datos**, así que existe corra o no el
contenedor. La petición llega al router, el proxy tiene destino 0 o falla al
marcar, Traefik ve un 502, el middleware `errors` dispara, y despierta. El
catch-all se vuelve redundante. Hay que **mantenerlo funcionando** para
direct-publish y para el modo de etiquetas —es una situación de «una
migración nunca quita comportamiento en silencio»— pero deja de ser
obligatorio.

### El coste honesto de esta elección

**El control plane entra en el camino de datos de cada petición de preview.**
Hoy, en modo Traefik, reiniciar `oxidd` no interrumpe el tráfico de los
entornos en absoluto. Con el proxy llevando tráfico entre nodos, un reinicio
del daemon corta las conexiones en vuelo y frena las nuevas hasta que los
bucles de accept se rebinden en `reconcile_startup_state`. Es una regresión
real para quien use varios nodos y **tiene que ir en las notas de versión, no
descubrirse**.

Dos mitigaciones, ambas deliberadamente fuera de alcance: traspaso de socket
con `SO_REUSEPORT` entre reinicios, o correr el proxy en el nodo — que es la
opción del agente reapareciendo. Ése es el disparador honesto para
construirlo, y merece estar escrito en el módulo de flota:

> *Cuando el ancho de banda o la ventana de reinicio del control plane se
> convierta en la limitación, ahí es cuando el agente se gana su
> complejidad.*

---

## 6. Cada bloqueo, resuelto

El titular: **con un solo proceso de control plane, casi todos siguen siendo
correctos tal cual**. No es suerte, es lo que compra la opción (a).

| Bloqueo | Veredicto |
|---|---|
| `LockKey::Branch(proyecto, rama)` | **Se queda igual.** Protege filas de entorno y el corte del proxy, que viven en el control plane, no en el nodo. Deliberadamente **no** gana `NodeId`: un redespliegue puede *mover* una rama de nodo, y este bloqueo es precisamente lo que hace ese movimiento atómico. |
| `LockKey::GitCache(proyecto)` | **Se queda igual, y sale reforzado.** Los builds envían un tar de contexto, así que el checkout nunca sale del control plane. Sigue habiendo un directorio de trabajo por proyecto, en una máquina. |
| `LockKey::Admission` | **Pasa a `LockKey::Admission(NodeId)`.** Sigue en proceso y sigue siendo correcto. Aprovechar para corregir su comentario: el bloqueo nunca fue la reserva — la reserva es que `committed_memory_mb` cuenta las filas en `building`. Ahora ese conteo lleva `AND e.node_id = ?`. Sin tabla de reservas. |
| `LockKey::ResourcePool(tipo, instancia)` | **Se queda, y su respaldo en base de datos ya está hecho** (`5dd5113`): el insert es condicional a que el slot siga libre. Era un fallo real, no una precaución. |
| `deploy_drain_lock` | **Ya eliminado** (`62e3d00`). La reclamación en base de datos es estrictamente más fuerte. |
| `ProxyRegistry` | **Sigue en proceso, y asciende** de «alternativa para direct-publish» a camino de datos de la flota. Su único estado durable, `public_port`, ya se persiste y se reconstruye al arrancar. Correcto con un control plane. |
| `RefreshCoalescer` (`Instant`) | **Se queda igual, y es legítimamente local al proceso.** Coalesce futuros de `git fetch` en vuelo contra la única caché en disco de *este* proceso. Un `Instant` local es correcto justamente porque lo que protege es local. |
| `ArcSwap<Cipher>` | **Sin cambios.** Un proceso, un cifrador. |

**Dos piezas que no estaban en esa lista y sí deben cambiar:**

- **`reconcile_startup_state`** contrasta hoy cada entorno contra el Docker
  local. Hay que agrupar por `node_id` y despachar al `ContainerPort` de ese
  nodo. Y un detalle que pasa a ser **crítico**: `oci.rs::container_status`
  devuelve `Missing` **sólo** ante un 404 real; un fallo de conexión se mapea
  a `OciError::Failure`, que el reconciliador ya mete en `errors` en vez de
  actuar. Es decir, **un nodo inalcanzable no marca sus entornos como
  destruidos**. Ese invariante pasa a ser crítico y merece un test que lleve
  su nombre.
- **`sweep`** recorre todos los entornos y llama a `apply_gc_action`, que
  llama a `oci.stop/remove`. Hay que enrutar por `env.node_id`, y una acción
  de GC contra un nodo `down` debe fallar ruidosamente hacia
  `summary.errors`, nunca marcar la fila en silencio.

---

## 7. Fuera de alcance, explícitamente

- **Distribución de imágenes.** `helpers.rs::image_name` produce la misma
  etiqueta en cada nodo y cada nodo construye su copia. Una rama que cambia
  de nodo reconstruye desde cero: lento pero **correcto**, y sin registry que
  operar. Un registry es la mejora obvia después; no colarla por la puerta de
  atrás.
- **Alta disponibilidad del control plane / store distribuido.** Ver §3.
- **Migración en vivo de un entorno entre nodos.** Mover una rama es
  redesplegar y cortar, que el camino sin caída ya hace. Nunca
  *checkpointing* de contenedores.
- **Volúmenes y entornos con estado.** Hoy `ContainerSpec` no monta ningún
  volumen; hacer que un entorno se pegue a un nodo por razones de datos es
  otra funcionalidad.
- **Autoescalado o aprovisionamiento de nodos.** Los registra un operador.
  Oxid no crea máquinas.
- **Claves de secretos por nodo.** Los secretos siguen cifrados en el control
  plane y se inyectan vía `ContainerSpec.env` sobre la conexión mTLS. El nodo
  nunca guarda `secret.key`. Decir con franqueza que las variables de entorno
  son legibles con `docker inspect` en el nodo — igual que hoy en uno solo.
- **Named pipes de Windows, transporte SSH.** bollard 0.17 no tiene
  `connect_with_ssh`; añadirlo es una dependencia nueva, y mTLS cubre el
  mismo terreno gratis.
- **Backup del material TLS de los nodos.** `service/backup.rs` fotografía el
  fichero SQLite; las *rutas* de los certificados están en la fila `nodes`,
  pero los ficheros son cosa del operador. Que lo diga el comentario de la
  migración.

---

## 8. Modos de fallo de la recomendación

1. **Un socket Docker sobre TCP equivale a root en el nodo.** mTLS acota
   quién, no qué. Documentar que los nodos de Oxid han de ser máquinas en las
   que ya confiarías al control plane. Rechazar `tcp://` sin rutas TLS salvo
   que el operador ponga `OXID_ALLOW_INSECURE_NODES=1`, siguiendo el
   precedente que ya existe con `OXID_ALLOW_OPEN_API`.
2. **El control plane es punto único de fallo** para el control y —por §5—
   también para el tráfico. Los entornos vivos sobreviven; desplegar,
   despertar, la GC, la API y el enrutado entre nodos, no.
3. **Una partición de red es indistinguible de un nodo muerto.** El diseño no
   hace nada ante `down`: ni expulsa ni reprograma. Las filas se quedan como
   están y `oxid ps` muestra los entornos en un nodo marcado `down`. Expulsar
   automáticamente ante una partición es como acabas con dos copias de una
   rama vivas peleándose por una URL.
4. **La admisión sigue siendo orientativa, ahora por nodo.** `docker info`
   reporta memoria total, no libre; la reserva real es el conteo de filas en
   `building`. Un contenedor que se pasa de su límite es problema del kernel,
   como hoy.
5. **La colocación es una decisión puntual.** Nada reequilibra. Un nodo que
   se llena simplemente deja de recibir deploys y la cola se acumula detrás.
   Se preserva la equidad FIFO, lo que significa que un deploy grande puede
   seguir bloqueando la cabecera de toda la flota — propiedad que ya existe,
   ahora más visible.
6. **`node.address` lo da el operador y no es verificable.** Una dirección
   mal puesta produce una rama que despliega bien y es inalcanzable. Se
   mitiga con el sondeo posterior al deploy: `wait_until_ready` contra
   `node.address:host_port` hace exactamente eso, así que el deploy falla con
   honestidad en vez de reportar verde.

---

## 9. Etapas

Estimaciones en semanas-persona para alguien fluido en este código,
**incluyendo tests al nivel que la suite existente ya exige**.

### Etapa 0 — Cola reclamable · ✅ **ENTREGADA** (`62e3d00`)

Sin ningún concepto de multi-nodo. Arregla un fallo cuya existencia
documentaba el propio código.

Entregado: migración `0019`, `claim_deploy_queue`/`renew_deploy_leases`/
`release_deploy_claim`, drenaje por reclamación con renovación de lease,
identidad de worker con nonce de arranque, eliminación de
`deploy_drain_lock`, y tres tests (reclamaciones disjuntas, lease caducado
reclamable / renovado no, liberación diferida). Verificado además contra un
daemon real: seis pushes simultáneos, seis entornos, cola vacía.

También entregado fuera de etapa, por ser el mismo tipo de fallo:
la reclamación condicional de slots de pool (`5dd5113`).

### Etapa 1 — Identidad de nodo, con un solo nodo todavía · **1 semana** · cero cambio de comportamiento

Cada fila aprende dónde vive, mientras la respuesta siempre es «aquí».

- Migración `0020_nodes.sql`.
- `oxid-core`: `domain/node.rs`; `Environment.node_id`; reexportes.
- `store.rs`: métodos de `NodeStore` (`list_nodes`, `get_node`,
  `upsert_node`, `set_node_state`); `ENV_COLUMNS` y los mapeadores.
- `service/fleet.rs` (nuevo): `Fleet<O: ContainerPort>` sobre
  `ArcSwap<HashMap<NodeId, NodeHandle<O>>>` — `arc-swap` ya es dependencia
  vía el cifrador del store, así que no entra ningún crate nuevo.
  `ControlPlane.oci: O` pasa a `fleet: Fleet<O>`, y **`ControlPlane::new`
  conserva su firma**, registrando el `oci` recibido como nodo 1. Esa sola
  decisión es lo que mantiene compilando sin tocar
  `service/control_plane/tests.rs` y `api/tests.rs`, y lo que entrega «una
  instalación existente sigue funcionando sin cambiar configuración».
- Reescribir los **29** sitios `self.oci.` de `service/control_plane/` a
  `self.oci_for(node)?`. Mecánico, pero toca todo: es el grueso de la
  semana, y la razón de que la etapa 1 valga una semana entera pese a no
  cambiar ningún comportamiento.
- `committed_memory_mb` gana parámetro `node_id`; `LockKey::Admission` gana
  `NodeId`.

> **Criterio de aceptación:** la suite existente pasa **sin modificarse**, y
> una instalación en marcha se actualiza sin cambiar configuración y sin
> diferencia observable. Si eso no se cumple, la etapa 1 no está hecha.

### Etapa 2 — Nodos remotos · **3–4 semanas**

- `adapter/oci.rs`: `DockerClient::connect_to(&Node)` despachando a
  `connect_with_defaults` / `connect_with_ssl` / `connect_with_http`. Unas 40
  líneas. **Ya verificado que compila** con las features actuales.
- `oxid-core`: `domain/services/placement.rs` — `place(&[NodeCapacity],
  request_mb, affinity: Option<NodeId>) -> Placement`, puro. Afinidad
  primero (un redespliegue se queda donde está, para que la caché de imagen
  esté caliente), después el de más memoria libre. Enteramente testeable sin
  E/S, que es justamente el motivo de ponerlo en el dominio.
- `deploy_at`: resolver nodo antes de la admisión, pasarlo por
  `run_and_activate`, persistir `env.node_id`.
- `check_admission`: `host_capacity()` por nodo y `committed_memory_mb` por
  nodo. `Admission::Queue` pasa a significar «ahora mismo no cabe en ningún
  sitio».
- `sweep` y `reconcile_startup_state` con alcance de nodo.
- API y CLI: `POST/GET/PATCH/DELETE /api/v1/nodes`,
  `oxid node add|ls|drain|rm`. Las rutas de nodos son de ámbito de nodo, así
  que un token con alcance de proyecto recibe 403 por el modelo de acceso ya
  existente — gratis, pero conviene afirmarlo en un test.
- `NodeStats` gana desglose por nodo; el panel lo muestra.
- Sondeo de salud en el planificador: ping a cada nodo, actualizar
  `last_seen_at`/`state`. **No debe tocar filas de entorno.**

Al final de la etapa 2 un segundo nodo puede correr entornos, alcanzables por
puerto en modo direct-publish. El enrutado sigue siendo manual.

### Etapa 3 — Enrutado de flota · **2–3 semanas**

- `oxid-core/domain/services/routing.rs::dynamic_config`, puro y testeado.
- `GET /api/v1/traefik/config`, autenticado por bearer.
- `docker-compose.yml` y `oci.rs::ensure_traefik`: añadir
  `--providers.http.*`. **Mantener activo el proveedor de etiquetas** — una
  instalación existente ha de seguir enrutando por etiquetas mientras no se
  escriba nada nuevo.
- Regenerar en cada deploy/pause/wake/destroy es innecesario (Traefik
  sondea), pero exponer un ETag para abaratar el sondeo.
- `oxid infra status` reporta el cableado del proveedor HTTP junto a las
  comprobaciones actuales, y deja de tratar como fatal la ausencia de
  `oxid-wake-catchall` cuando el proveedor HTTP está vivo.

### Etapa 4 — Operación · **1–2 semanas**

`oxid node drain` moviendo ramas por redespliegue y corte; memoria reservada
por nodo; eventos de auditoría atribuyendo la colocación; documentación en
`PRODUCTION.md` sobre generar certificados TLS de Docker y sobre el
compromiso de punto único de fallo y camino de datos.

**Total restante: aproximadamente 7–10 semanas.**

---

## 10. Verificación

Lo que no debe romperse, con las cifras que hay que **volver a medir**, no
suponer (`BENCHMARKS.md`):

| Medida | Referencia |
|---|---|
| 15 pushes simultáneos | 7,1 s (4,2 s con `OXID_DEPLOY_CONCURRENCY=16`) |
| Heartbeat de Traefik | 948–5108 req/s, p50 < 12 ms |
| Rebuild incremental | 2 s con caché de BuildKit |
| Despertar un entorno dormido | 1–2 s |

Y el ciclo completo que ya se prueba a mano —instalar, registrar, desplegar
sin Dockerfile, servir por Traefik, dormir por inactividad, despertar por
petición, webhook firmado con filtro de ramas, matriz de roles, secretos que
llegan al contenedor, suspender y reanudar— tiene que seguir pasando entero.

Para las etapas 2 y 3 hace falta además un escenario de dos nodos reales. La
forma barata: dos daemons Docker en la misma máquina, el segundo escuchando
en TCP con certificados generados para la prueba. No es un sustituto de dos
máquinas, pero cubre todo salvo la latencia de red y la partición — y para
esos dos, `tc netem` da lo que falta.

---

## 11. Ficheros que tocará

| Fichero | Qué cambia |
|---|---|
| `service/control_plane/mod.rs` | `oci` → `Fleet`, `LockKey::Admission(NodeId)` |
| `adapter/store.rs` | tabla `nodes`, `ENV_COLUMNS`, mapeadores |
| `service/control_plane/deploy.rs` | colocación y admisión en `deploy_at` |
| `service/control_plane/gc.rs` | `sweep` y `reconcile_startup_state` por nodo |
| `adapter/oci.rs` | `connect_to`; `container_status` es el invariante que impide que un nodo inalcanzable destruya sus entornos |
| `oxid-core/src/domain/environment.rs` | `node_id` en la entidad |
| `oxid-core/src/domain/node.rs` | nuevo |
| `oxid-core/src/domain/services/placement.rs` | nuevo, puro |
| `oxid-core/src/domain/services/routing.rs` | nuevo, puro |
| `service/proxy.rs` | marcar `node.address` en vez de `127.0.0.1` |

---

## 12. El impuesto que paga cada etapa

No es opcional en este repositorio, y conviene contarlo al estimar:

- **i18n por triplicado.** Cada mensaje visible por una persona va en inglés
  y español a `oxid-daemon/src/i18n.rs`, `oxid-cli/src/i18n.rs` y/o
  `web/i18n.js`. Hay tests que fallan ante una clave que falta o un marcador
  inventado.
- **Frontera hexagonal.** `oxid-core` no puede ganar
  `tokio|sqlx|bollard|axum|reqwest|git2|hyper|tower|tar`, verificado por
  hooks y CI. Las reglas van al dominio como servicios puros; los
  adaptadores, fuera.
- **La puerta completa**: `fmt --check` → `clippy -D warnings` → `test
  --workspace`, en hooks y en CI, más el job `integration` que ejecuta los
  tests contra Docker y Postgres reales.
- **Los dobles de test.** Cada método nuevo de `ContainerPort` hay que
  añadirlo a los `FakeOci` de `service/control_plane/tests.rs` y
  `api/tests.rs` **en el mismo commit**, o el workspace deja de compilar.
- **Documentación.** El sitio `docs/` es HTML escrito a mano con la barra
  lateral duplicada por página. Tocar la topología deja esas páginas
  obsoletas.
