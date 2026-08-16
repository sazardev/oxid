Aquí tienes el `SPEC.md` diseñado bajo una arquitectura de software robusta. Este documento plantea un producto altamente opinionado: **todo está integrado (pilas incluidas), estructurado bajo principios de Clean Architecture y optimizado para exprimir hasta el último megabyte de RAM** utilizando Rust.

---

# SPEC.md: Orquestador de Entornos Efímeros de Alto Rendimiento

## 1. Visión del Producto y Principios de Diseño

Este sistema es una plataforma _self-hosted_ para la orquestación inteligente de ramas _feature_ (Trunk-based / GitOps). Actúa como un plano de control (_Control Plane_) que intercepta eventos de repositorios, construye imágenes, inyecta configuraciones y despliega contenedores bajo demanda.

**Principios Core:**

- **Eficiencia Absoluta:** Escrito 100% en Rust. Huella de memoria mínima (< 15MB en reposo).
- **Scale-to-Zero Activo:** Los entornos que no reciben tráfico HTTP se pausan en memoria o se detienen. Se reactivan en milisegundos ante la primera petición entrante.
- **Multiplexación de Recursos:** Inteligencia para compartir dependencias pesadas (ej. bases de datos, cachés) entre múltiples ramas para evitar el colapso del nodo local.
- **Agnóstico pero Opinionado:** Soporta cualquier stack contenedorizado, pero impone convenciones estrictas en el ruteo, manejo de secretos y ciclo de vida.
- **Ecosistema Unificado:** No requiere herramientas de terceros. Contiene su propia base de datos (SQLite embebido), su propio motor de colas y su propio servidor web.

---

## 2. Arquitectura del Sistema (Ports & Adapters)

El núcleo del sistema está diseñado utilizando **Arquitectura Hexagonal (Ports & Adapters)** para separar estrictamente las reglas de orquestación de los detalles de infraestructura.

### 2.1. Capa de Dominio (Core Logic)

- **Entidades:** `Project`, `Branch`, `Environment`, `ResourcePool`, `SecretContext`.
- **Reglas de Negocio:**
- Un `Environment` solo puede estar en un estado a la vez (`Building`, `Running`, `Paused`, `Hibernating`, `Destroyed`).
- Las variables de entorno se calculan por una matriz de herencia: `Global -> Project -> Branch -> Runtime`.
- La recolección de basura (_Garbage Collection_) se dispara automáticamente según el TTL (Time-To-Live) configurado por proyecto.

### 2.2. Puertos (Interfaces) y Adaptadores

- **Orquestación OCI:** Adaptador asíncrono utilizando `bollard` para interactuar directamente con el socket UNIX de Docker (`/var/run/docker.sock`).
- **Persistencia:** Adaptador basado en `sqlx` con SQLite para transacciones ultrarrápidas, historial de despliegues y auditoría local sin requerir un servidor de BD externo.
- **Versionamiento:** Adaptador basado en `git2` para manejo nativo y concurrente de repositorios (clonación en caché, checkouts sin cabeza).
- **Red y Webhooks:** Adaptador HTTP implementado con `axum` y `tokio` para la ingesta de webhooks y la API REST interna.

---

## 3. Lógica de Ahorro de Recursos e Inteligencia

### 3.1. Multiplexación de Dependencias (_Smart Resource Sharing_)

En lugar de levantar un clúster de dependencias (Redis, Postgres, RabbitMQ) por cada rama, el sistema implementa **Resource Pools**:

- **Bases de Datos Relacionales (Postgres/MySQL):** El sistema mantiene **un solo contenedor** de base de datos encendido. Cuando se levanta la rama `feature-A`, el orquestador se conecta al contenedor raíz, crea un _schema_ o base de datos lógica dedicada (`db_feature_a`) e inyecta esa URL de conexión específica al contenedor de la aplicación.
- **Caché (Redis):** Se comparte una única instancia de Redis. El orquestador inyecta una variable de entorno para que cada rama use un índice de base de datos distinto (`REDIS_DB=1`, `REDIS_DB=2`), o utiliza prefijos automáticos en las claves.

### 3.2. Estrategia "Scale-to-Zero"

1. **Monitor de Tráfico:** Un proxy inverso (Traefik) actúa como entrada.
2. **Hibernación por Inactividad:** Un _cron_ interno de Rust evalúa la actividad de red. Si la rama `feature-x` no recibe peticiones en 30 minutos, ejecuta `docker pause feature-x`.
3. **Despertar bajo demanda:** Traefik está configurado para redirigir peticiones fallidas (por contenedor pausado) a un endpoint especial del orquestador en Rust. El orquestador hace `docker unpause` (latencia ~300ms) y devuelve una señal de recarga al navegador. El usuario percibe una ligera latencia en la primera carga, pero el sistema ahorra gigabytes de RAM.

---

## 4. Flujo Principal de Ejecución (Pipeline)

El flujo se define de manera declarativa mediante un archivo `orchestrator.yml` (o auto-detectado):

1. **Ingesta (Trigger):** `axum` recibe un `push webhook` de GitHub/GitLab. Se verifica el payload criptográficamente.
2. **Locking Concurrente:** Se adquiere un bloqueo transaccional en SQLite para evitar condiciones de carrera si entran múltiples _pushes_ a la misma rama simultáneamente.
3. **Sincronización (Git):** Se extrae el delta de código. Se utiliza una caché global de repositorios para clonar de forma casi instantánea usando enlaces duros (_hard links_) en el sistema de archivos local.
4. **Matriz de Inyección:** Se compilan los secretos cacheados en disco (encriptados mediante `AES-GCM` usando una clave maestra local) y se inyectan dinámicamente.
5. **Construcción (Build):** Se ejecuta el contenedor constructor. Se aplican técnicas de caché de capas (BuildKit) y volúmenes compartidos para dependencias (ej. `~/.m2`, `~/.cargo/registry`, `node_modules` en volúmenes Docker huérfanos mapeados automáticamente).
6. **Despliegue y Enrutamiento:** Se levanta el contenedor con etiquetas específicas del proxy. Ejemplo: `Subdominio -> feat-login.dev.local`.
7. **Notificación de Auditoría:** Registro en la base de datos de eventos (logs de compilación, latencias, estado de RAM) y emisión por WebSocket hacia los clientes.

---

## 5. Interfaces y Superficie de Control (Pilas Incluidas)

Para garantizar un flujo de trabajo fluido, el sistema se maneja desde múltiples interfaces que consumen la misma API interna:

### 5.1. CLI (Command Line Interface)

Para control rápido de bajo nivel y scripting.

- `$ ctrl up my-repo --branch feature-login` -> Fuerza un despliegue manual.
- `$ ctrl env set my-repo DB_PASSWORD=secret --scope global` -> Inyecta variables.
- `$ ctrl logs feature-login -f` -> Streaming de logs de la rama.

### 5.2. TUI (Terminal User Interface)

Interfaz de terminal interactiva centrada en el teclado (inspirada en flujos de trabajo como Tmux/Lazygit/Neovim).

- Visualización de árboles de ramas, uso de CPU/RAM en tiempo real por contenedor.
- Acciones de pausa/reinicio y atajos de teclado para entrar a la _shell_ del contenedor (`exec -it`).

### 5.3. Web Dashboard

Incluido dentro del mismo binario estático de Rust (archivos precompilados e incrustados).

- Métricas globales del nodo, auditoría histórica de despliegues y visor de logs estructurados.

### 5.4. Desktop App (Tauri + React)

Aplicación de escritorio opcional, construida sobre **Tauri**, que se conecta al daemon de Rust en red local.

- Permite a los desarrolladores o QA ver qué ramas están vivas, hacer clic para abrir la URL efímera en el navegador, y ver el estado de los recursos de la máquina sin tocar la terminal.

---

## 6. Configuración y Self-Hosting

El diseño es de **Despliegue Cero-Fricción**. Todo el sistema es un binario único (estático).

- **Ejecución de arranque:**

```bash
# Ejecución directa montando el socket de Docker y un directorio para datos
docker run -d --name control-plane \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v /opt/data:/data \
  -p 8080:8080 \
  ghcr.io/tu-usuario/orchestrator:latest

```

- **Estructura del directorio `/data`:**
- `/data/config.toml` (Configuración de dominios, tokens, reglas de recolección de basura).
- `/data/audit.sqlite` (Base de datos transaccional con WAL mode habilitado para extrema velocidad).
- `/data/git-cache/` (Repositorios cacheados para builds rápidos).

---

¿Qué módulo o componente específico de este SPEC te gustaría desarrollar primero a nivel de código (por ejemplo, el motor de proxy asíncrono, la lógica de multiplexación de dependencias en SQLite, o el adaptador OCI con Bollard)?
