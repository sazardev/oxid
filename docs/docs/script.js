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

  // Scroll-spy: highlight the sidebar entry of the section currently in view.
  const links = [...document.querySelectorAll(".docs-sidebar a[href^='#']")];
  const byId = new Map(links.map((l) => [l.getAttribute("href").slice(1), l]));

  if ("IntersectionObserver" in window && byId.size > 0) {
    const observer = new IntersectionObserver(
      (entries) => {
        entries.forEach((entry) => {
          if (!entry.isIntersecting) return;
          links.forEach((l) => l.classList.remove("active"));
          const link = byId.get(entry.target.id);
          if (link) link.classList.add("active");
        });
      },
      // A section counts as "current" while it crosses the upper-middle band
      // of the viewport — keeps exactly one link lit while scrolling.
      { rootMargin: "-15% 0px -65% 0px" }
    );
    byId.forEach((_link, id) => {
      const section = document.getElementById(id);
      if (section) observer.observe(section);
    });
  }
})();
