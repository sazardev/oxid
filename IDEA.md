Aquí tienes el `IDEA.md` estructurado para capturar la visión, filosofía y posicionamiento de **Oxid**. Este documento sirve como el "manifiesto" de tu producto, ideal para mantener el foco durante el desarrollo o presentarlo a otros desarrolladores.

---

# IDEA.md: Oxid

> **"Entornos efímeros que respiran. Rendimiento férreo, huella invisible."**

## 1. El Nombre: Oxid

El nombre **Oxid** proviene de la combinación conceptual entre **Óxido (Oxide)** y **Oxígeno**.

- **Inspiración en Rust:** Es un tributo directo a su lenguaje base (Rust significa óxido). Evoca la seguridad del metal a bajo nivel, la robustez industrial y la filosofía de "cero caídas".
- **Ligereza del Oxígeno:** Refleja su comportamiento ágil. Así como el oxígeno da vida, Oxid "respira vida" en las ramas de Git levantando entornos al instante. Y al igual que un gas, se expande cuando lo necesitas y se contrae (Scale-to-Zero) cuando nadie lo usa, sin dejar rastro en la RAM.

## 2. La Gran Idea (El Problema y la Solución)

**El Problema:** Levantar entornos de prueba por cada rama (_feature branches_) usualmente significa pagar fortunas a servicios Cloud (Vercel, Heroku, Render) o colapsar el servidor local/NAS levantando decenas de contenedores Docker que nadie está usando activamente.
**La Solución:** Oxid es un plano de control _self-hosted_ y opinionado. Detecta un _push_, inyecta variables, levanta el contenedor, lo conecta a un proxy y, **si nadie lo visita en 30 minutos, lo hiberna en memoria**. La próxima vez que alguien entra a la URL, Oxid lo despierta en milisegundos. Es el Vercel de los servidores locales, pero con el consumo de recursos de una calculadora.

## 3. Filosofía y Enfoque (Producto Opinionado)

Oxid no intenta ser un Kubernetes. Es una herramienta con "pilas incluidas" que toma decisiones por ti para garantizar la máxima eficiencia:

- **Fricción Cero:** Nada de escribir largos manifiestos YAML. Si hay un `Dockerfile` y un `docker-compose.yml`, Oxid sabe qué hacer.
- **Avaricia de Recursos:** Oxid asume que tu CPU y RAM son oro. Por defecto, multiplexa bases de datos (un solo Postgres, múltiples esquemas efímeros) y pausa agresivamente los contenedores inactivos.
- **Todo es local, todo está auditado:** No depende de bases de datos externas. Usa SQLite transaccional integrado para guardar cada log, cada despliegue y cada variable inyectada.

## 4. Coherencia de Usabilidad (Las Interfaces)

El producto debe sentirse como una extensión natural de las manos del desarrollador. No hay cambio de contexto disruptivo.

| Interfaz               | Propósito                                  | Experiencia de Usuario (UX)                                                                                                                |
| ---------------------- | ------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------ |
| **CLI (`oxid`)**       | Control rápido e integración en scripts    | Comandos cortos, salida coloreada e intuitiva. Ej: `oxid up feature-login`, `oxid env set`.                                                |
| **TUI (Terminal UI)**  | Monitoreo táctico sin salir de la terminal | Navegación con teclado (estilo `lazygit`). Árbol de ramas a la izquierda, uso de RAM y logs en vivo a la derecha.                          |
| **Escritorio (Tauri)** | Visualización amigable para QA y Managers  | Una app nativa ligerísima en la barra de tareas. Un clic para abrir la URL de una rama, ver estados (Verde/Gris/Rojo) y compartir accesos. |

## 5. Marketing y Posicionamiento

Si Oxid fuera un producto comercial (o un proyecto Open Source buscando adopción masiva), este sería su ángulo de venta:

- **El Gancho:** "Deja de pagar por entornos de _staging_ que duermen el 90% del tiempo."
- **El Superpoder técnico:** "Escrito en Rust. Consume 8MB de RAM. Orquesta miles de ramas locales sin sudar."
- **El Target:** Desarrolladores Full-Stack, ingenieros DevOps frustrados con la lentitud de Jenkins/K8s, y equipos pequeños/medianos que hacen despliegues continuos (_Trunk-based development_).
- **El Tono de la Marca:** Oscuro, técnico, minimalista, rápido. Colores de marca: Negro carbón, Naranja óxido (Rust) y Blanco brillante.

## 6. Flujo de Coherencia (El "Golden Path")

La magia de Oxid se basa en que el usuario nunca siente la complejidad interna:

1. **Día 1:** El usuario instala Oxid ejecutando un solo binario. Configura un dominio comodín (`*.local.dev`).
2. **Día 2:** El desarrollador crea la rama `feature-carrito` y hace `git push`.
3. **La Magia:** En 5 segundos, Oxid clona la rama localmente, compila si es necesario, inyecta variables secretas y despliega. Traefik enruta automáticamente a `carrito.local.dev`.
4. **Día 3:** La rama lleva 2 días sin revisarse. Oxid apagó el contenedor, liberando 500MB de RAM. Un tester entra a la URL, Oxid lo nota, hace `unpause` en 200ms y el tester navega como si nada hubiera pasado.

---

Got it. From now on, everything will be in English.

To keep our momentum, let's look at the next logical step for **Oxid**: designing the `oxid.toml` configuration file. This file acts as the contract between the developer's repository and the Oxid control plane.

---

# The `oxid.toml` Specification

The beauty of Oxid is that it requires minimal configuration, relying heavily on sensible defaults. If a repository has a `Dockerfile`, Oxid can deploy it. However, placing an `oxid.toml` file in the root directory unlocks its advanced features, like smart resource multiplexing and aggressive scale-to-zero tuning.

Here is the ideal structure of the configuration file:

```toml
# oxid.toml

[project]
name = "my-awesome-api"
# How long the environment must be idle before Oxid executes `docker pause`
pause_after = "30m"
# How long before Oxid completely destroys the container and its ephemeral volumes
destroy_after = "7d"

[build]
# Oxid defaults to "Dockerfile" in the root, but you can override it
dockerfile = "deploy/Dockerfile.dev"
context = "."
# Command injected to run once the container starts (useful for ephemeral data)
on_start = ["npm run db:migrate", "npm run db:seed"]

[routing]
# The base subdomain pattern. Oxid will prepend the branch name.
# Example: feature-login.my-awesome-api.local.dev
base_domain = "my-awesome-api.local.dev"
# The internal port Traefik should route traffic to
port = 8080

[dependencies]
# This is where Oxid's smart multiplexing shines.
# Instead of spinning up a new DB container, it uses the shared local instance.

  [dependencies.database]
  type = "postgres"
  # Refers to a master connection Oxid manages globally
  shared_instance = "local-pg-cluster"
  # Oxid creates a unique logical DB (e.g., `db_feature_login`)
  # and injects the connection string into this environment variable
  inject_url_as = "DATABASE_URL"

  [dependencies.cache]
  type = "redis"
  shared_instance = "local-redis-cluster"
  inject_url_as = "REDIS_URL"

```

## How Oxid Interprets This File (The Business Logic)

1. **The `[project]` Block (Scale-to-Zero):**
   Oxid’s internal Tokio scheduler reads `pause_after = "30m"`. It continuously monitors Traefik's access metrics for this specific branch. If 30 minutes pass with no HTTP requests, it suspends the container in memory.
2. **The `[build]` Block (Immutability):**
   The `on_start` hook is crucial. Since Oxid doesn't clone production databases to save resources, developers use this hook to run migrations and seed lightweight mock data every time the ephemeral environment boots up.
3. **The `[dependencies]` Block (Resource Multiplexing):**
   When Oxid sees the `database` block, it runs a pre-flight routine: it connects to the `local-pg-cluster`, executes `CREATE DATABASE db_<branch_name>;`, constructs the new DSN string, and injects it into the container's environment as `DATABASE_URL`.

---
