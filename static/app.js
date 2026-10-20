// OpenFault UI: theme, copy, keyboard, remembered filters, live counters, TV. Everything works without it.
(() => {
  const d = document, root = d.documentElement;
  const $ = (s, r = d) => r.querySelector(s), $$ = (s, r = d) => [...r.querySelectorAll(s)];
  const store = (k, v) => { try { v == null ? localStorage.removeItem(k) : localStorage.setItem(k, v) } catch (_) {} };
  const load = k => { try { return localStorage.getItem(k) } catch (_) { return null } };

  // last-used issue filters, per list (ponytail: browser-only, never synced)
  if (/\/issues$/.test(location.pathname)) {
    const key = "of-issues:" + location.pathname, p = new URLSearchParams(location.search);
    p.delete("cursor");
    if (location.search) store(key, "?" + p);
    else if (load(key)) return location.replace(location.pathname + load(key));
  }

  const calm = () => matchMedia("(prefers-reduced-motion: reduce)").matches;
  // MPA view transitions only between tabs of one page (same path, or within settings); elsewhere navigate instantly
  addEventListener("pagereveal", e => {
    if (!e.viewTransition) return;
    let a = ""; try { a = new URL(d.referrer).pathname } catch (_) {}
    const b = location.pathname, s = x => x.startsWith("/settings");
    if (a !== b && !(s(a) && s(b))) e.viewTransition.skipTransition();
  });
  const mark = () => $$("[data-theme-set]").forEach(b => {
    const on = b.dataset.themeSet === (root.dataset.theme || "");
    b.classList.toggle("on", on); b.setAttribute("aria-checked", on);
  });
  if (!$(".tv")) mark();
  // sidebar sections remember collapsed state
  $$("details.grp[data-sec]").forEach(g => {
    const k = "of-side:" + g.dataset.sec;
    if (load(k) === "0") g.open = false;
    g.addEventListener("toggle", () => store(k, g.open ? null : "0"));
  });

  // charts animate on first paint only; live swaps just update
  setTimeout(() => root.classList.add("settled"), 1200);

  const copy = (t, b) => navigator.clipboard.writeText(t).then(() => {
    const o = b.innerHTML; b.textContent = "Copied"; setTimeout(() => b.innerHTML = o, 1500);
  });

  d.addEventListener("click", e => {
    const t = e.target.closest("[data-theme-set]");
    if (t) {
      const v = t.dataset.themeSet;
      store("of-theme", v || null);
      const set = () => { v ? root.dataset.theme = v : delete root.dataset.theme; mark() };
      if (!d.startViewTransition || calm()) set();
      else { root.classList.add("vt-theme"); d.startViewTransition(set).finished.finally(() => root.classList.remove("vt-theme")) }
    }
    const c = e.target.closest("[data-copy]");
    if (c) copy($("pre", c.parentElement).innerText, c);
    const u = e.target.closest("[data-copy-url]");
    if (u) { e.preventDefault(); fetch(u.dataset.copyUrl).then(r => r.text()).then(x => copy(x, u)) }
    $$("details.dd[open]").forEach(x => { if (!x.contains(e.target)) x.open = false });
  });
  d.addEventListener("change", e => {
    if (e.target.matches("[data-autosubmit]")) e.target.form.requestSubmit();
    if (e.target.id === "s-t" && e.target.checked) setTimeout(() => $("#gq").focus(), 0);
  });

  // ---- live: connection dot, counters that count, rows that slide in ----
  const live = on => $$("#live").forEach(l => l.classList.toggle("on", on));
  d.addEventListener("htmx:sseOpen", () => live(true));
  d.addEventListener("htmx:sseError", () => live(false));
  d.addEventListener("htmx:sseClose", () => live(false));
  // dashboard/TV: a faint 1px sweep per tick
  const bar = ($("#dash") || $(".tv")) && d.body.appendChild(d.createElement("i"));
  if (bar) bar.className = "tickbar";
  d.addEventListener("htmx:sseMessage", () => bar && !calm() && bar.animate([{ transform: "scaleX(0)", opacity: .4 }, { transform: "scaleX(1)", opacity: .4, offset: .8 }, { transform: "scaleX(1)", opacity: 0 }], { duration: 300, easing: "cubic-bezier(.2,.8,.2,1)" }));
  let snap = null;
  d.addEventListener("htmx:beforeSwap", e => {
    const t = e.detail.target;
    if (!t || t.id === "side-p" || !t.matches("[hx-trigger^='sse:']")) return;
    snap = { n: {}, k: new Set($$("[data-k]", t).map(x => x.dataset.k)) };
    $$("[data-n][id]", t).forEach(x => snap.n[x.id] = +x.dataset.n);
  });
  const fmt = v => Math.round(v).toLocaleString("en-US");
  const count = (el, a, b) => {
    const t0 = performance.now();
    el.classList.add("bump");
    const step = t => {
      const p = Math.min((t - t0) / 400, 1);
      el.textContent = fmt(a + (b - a) * (1 - Math.pow(1 - p, 3)));
      if (p < 1) requestAnimationFrame(step); else el.classList.remove("bump");
    };
    requestAnimationFrame(step);
  };
  d.addEventListener("htmx:afterSettle", () => {
    if (!snap) return;
    const s = snap; snap = null;
    for (const [id, a] of Object.entries(s.n)) {
      const el = d.getElementById(id);
      if (el && +el.dataset.n !== a && !calm()) count(el, a, +el.dataset.n);
    }
    if (s.k.size) $$("[data-k]").forEach(x => { if (!s.k.has(x.dataset.k)) x.classList.add("fresh") });
    tvPage();
  });

  // ---- TV: clock, project paging, fullscreen ----
  const tv = $(".tv");
  let page = 0;
  function tvPage() {
    const g = $("#tvp"); if (!g) return;
    const cards = $$(".tv-card", g), per = 6, n = Math.ceil(cards.length / per);
    page = n ? page % n : 0;
    cards.forEach((c, i) => c.hidden = n > 1 && Math.floor(i / per) !== page);
  }
  if (tv) {
    const clock = () => { const c = $("#clock"); if (c) c.textContent = new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }) };
    clock(); setInterval(clock, 1000); tvPage();
    setInterval(() => {
      const g = $("#tvp"); if (!g || $$(".tv-card", g).length <= 6) return;
      g.classList.add("fade");
      setTimeout(() => { page++; tvPage(); g.classList.remove("fade") }, 450);
    }, 20000);
  }

  const search = () => { const s = $("#q") || $("#gq"); s.focus(); s.select() };
  const A = { r: "resolve", m: "mute", a: "approve" };
  const act = (id, a) => {
    const f = d.createElement("form");
    f.method = "post"; f.action = `/issues/${id}/action`;
    for (const [n, v] of [["action", a], ["next", location.pathname + location.search]]) {
      const i = d.createElement("input"); i.type = "hidden"; i.name = n; i.value = v; f.append(i);
    }
    d.body.append(f); f.submit();
  };

  d.addEventListener("keydown", e => {
    const k = e.key, el = e.target;
    const typing = /^(INPUT|TEXTAREA|SELECT)$/.test(el.tagName) || el.isContentEditable;
    if ((e.metaKey || e.ctrlKey) && k === "k") { e.preventDefault(); return search() }
    if (k === "Escape") {
      $$("details.dd[open]").forEach(x => x.open = false);
      $$("#s-t,#nav-t").forEach(x => x.checked = false);
      if (typing) el.blur();
      return;
    }
    if (typing || e.metaKey || e.ctrlKey || e.altKey) return;
    if (tv && (k === "f" || k === "F")) return d.fullscreenElement ? d.exitFullscreen() : root.requestFullscreen().catch(() => {});
    if (k === "/") { e.preventDefault(); return search() }
    if (k === "?") { const h = $("#help"); if (h) { e.preventDefault(); h.showModal() } return }
    const rows = $$(".row[data-id]");
    if (!rows.length) return;
    const cur = el.closest && el.closest(".row[data-id]");
    let i = rows.indexOf(cur);
    if (k === "j" || k === "k") {
      e.preventDefault();
      i = k === "j" ? Math.min(i + 1, rows.length - 1) : Math.max(i - 1, 0);
      rows[i].focus();
      rows[i].scrollIntoView({ block: "nearest" });
    } else if (!cur) return;
    else if (k === "Enter" && el === cur) location = $("a.t", cur).href;
    else if (k === "x") { const c = $(".sel", cur); c.checked = !c.checked; c.dispatchEvent(new Event("change", { bubbles: true })) }
    else if (A[k]) act(cur.dataset.id, A[k]);
  });
})();
