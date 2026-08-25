(() => {
  const toggle = document.getElementById("navToggle");
  const nav = document.getElementById("primaryNav");

  if (toggle && nav) {
    toggle.addEventListener("click", () => {
      const isOpen = nav.classList.toggle("is-open");
      toggle.setAttribute("aria-expanded", String(isOpen));
    });

    nav.querySelectorAll("a").forEach((link) => {
      link.addEventListener("click", () => {
        nav.classList.remove("is-open");
        toggle.setAttribute("aria-expanded", "false");
      });
    });
  }

  const copyable = document.querySelectorAll(".qs-cmd");
  copyable.forEach((el) => {
    el.title = "Click to copy";
    el.addEventListener("click", async () => {
      const text = el.textContent.replace(/^\$\s?/, "").trim();
      try {
        await navigator.clipboard.writeText(text);
      } catch {
        const ta = document.createElement("textarea");
        ta.value = text;
        ta.setAttribute("readonly", "");
        ta.style.position = "fixed";
        ta.style.opacity = "0";
        document.body.appendChild(ta);
        ta.select();
        document.execCommand("copy");
        ta.remove();
      }
      el.classList.add("qs-copied");
      setTimeout(() => el.classList.remove("qs-copied"), 1200);
    });
  });

  // ---------- Showcase tabs (CLI / dashboard preview) ----------
  const tabs = document.querySelectorAll(".showcase-tab");
  const panels = document.querySelectorAll(".showcase-panel");

  tabs.forEach((tab) => {
    tab.addEventListener("click", () => {
      tabs.forEach((t) => {
        t.classList.toggle("is-active", t === tab);
        t.setAttribute("aria-selected", String(t === tab));
      });
      panels.forEach((panel) => {
        const active = panel.dataset.panel === tab.dataset.target;
        panel.classList.toggle("is-active", active);
        panel.hidden = !active;
        if (active) startTypewriter(panel);
      });
    });
  });

  // ---------- Typewriter effect ----------
  const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  const startedTypewriters = new WeakSet();

  function startTypewriter(scope) {
    const container = scope.querySelector("[data-typewriter]");
    if (!container || startedTypewriters.has(container)) return;
    startedTypewriters.add(container);

    const lines = Array.from(container.querySelectorAll(".tw-line"));
    if (!lines.length) return;

    if (reducedMotion) {
      container.classList.add("is-typing");
      return;
    }

    lines.forEach((line) => {
      line.dataset.full = line.textContent;
      line.textContent = "";
    });
    container.classList.add("is-typing");

    let lineIndex = 0;

    const typeLine = () => {
      const line = lines[lineIndex];
      const full = line.dataset.full;
      const caret = document.createElement("span");
      caret.className = "tw-caret";
      let charIndex = 0;

      const typeChar = () => {
        line.textContent = full.slice(0, charIndex);
        line.appendChild(caret);
        charIndex += 1;
        if (charIndex <= full.length) {
          setTimeout(typeChar, 16 + Math.random() * 30);
        } else {
          caret.remove();
          lineIndex += 1;
          if (lineIndex < lines.length) {
            setTimeout(typeLine, 260);
          }
        }
      };
      typeChar();
    };

    typeLine();
  }

  // Auto-start the CLI panel's typewriter once it's on screen.
  const heroTypewriter = document.querySelector(".showcase-panel.is-active");
  if (heroTypewriter && "IntersectionObserver" in window) {
    const twObserver = new IntersectionObserver(
      (entries) => {
        entries.forEach((entry) => {
          if (entry.isIntersecting) {
            startTypewriter(entry.target);
            twObserver.unobserve(entry.target);
          }
        });
      },
      { threshold: 0.3 }
    );
    twObserver.observe(heroTypewriter);
  } else if (heroTypewriter) {
    startTypewriter(heroTypewriter);
  }

  const revealTargets = document.querySelectorAll(
    ".card, .feature, .timeline li, .interface-card"
  );
  revealTargets.forEach((el) => el.classList.add("reveal"));

  if ("IntersectionObserver" in window) {
    const observer = new IntersectionObserver(
      (entries) => {
        entries.forEach((entry) => {
          if (entry.isIntersecting) {
            entry.target.classList.add("is-visible");
            observer.unobserve(entry.target);
          }
        });
      },
      { threshold: 0.15 }
    );
    revealTargets.forEach((el) => observer.observe(el));
  } else {
    revealTargets.forEach((el) => el.classList.add("is-visible"));
  }
})();
