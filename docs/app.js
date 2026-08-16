const root = document.documentElement;
const body = document.body;
const locale = root.lang || "en";
const uiStrings = Object.freeze({
  themeLightLabel: body.dataset.themeLightLabel,
  themeDarkLabel: body.dataset.themeDarkLabel,
  searchEmptyTemplate: body.dataset.searchEmptyTemplate,
  copySuccess: body.dataset.copySuccess,
});
const sections = [...document.querySelectorAll(".doc-section[id]")];
const navLinks = [...document.querySelectorAll('.sidebar a[href^="#"]')];
const menuToggle = document.querySelector(".menu-toggle");
const sidebar = document.querySelector(".sidebar");
const mobileCurrent = document.querySelector(".mobile-current");
const themeToggle = document.querySelector(".theme-toggle");
const themeIcon = document.querySelector(".theme-icon");
const searchDialog = document.querySelector(".search-dialog");
const searchInput = document.querySelector("#doc-search");
const searchResults = document.querySelector(".search-results");
const searchTrigger = document.querySelector(".search-trigger");
const backToTop = document.querySelector(".back-to-top");
const toast = document.querySelector(".toast");

const searchIndex = sections.map((section) => ({
  id: section.id,
  title: section.dataset.title || section.querySelector("h2")?.textContent || section.id,
  text: section.textContent.replace(/\s+/g, " ").trim(),
}));

function setTheme(theme, persist = true) {
  root.dataset.theme = theme;
  themeIcon.textContent = theme === "dark" ? "☼" : "◐";
  themeToggle.setAttribute(
    "aria-label",
    theme === "dark" ? uiStrings.themeLightLabel : uiStrings.themeDarkLabel,
  );
  document.querySelector('meta[name="theme-color"]').content = theme === "dark" ? "#111318" : "#f2f0e8";
  if (persist) localStorage.setItem("sampler-docs-theme", theme);
}

const storedTheme = localStorage.getItem("sampler-docs-theme");
const preferredTheme = matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
setTheme(storedTheme || preferredTheme, false);

themeToggle.addEventListener("click", () => {
  setTheme(root.dataset.theme === "dark" ? "light" : "dark");
});

function closeMenu() {
  menuToggle.setAttribute("aria-expanded", "false");
  sidebar.classList.remove("open");
  body.classList.remove("menu-open");
}

menuToggle.addEventListener("click", () => {
  const opening = menuToggle.getAttribute("aria-expanded") !== "true";
  menuToggle.setAttribute("aria-expanded", String(opening));
  sidebar.classList.toggle("open", opening);
  body.classList.toggle("menu-open", opening);
});

navLinks.forEach((link) => link.addEventListener("click", closeMenu));

document.addEventListener("click", (event) => {
  if (!sidebar.classList.contains("open")) return;
  if (sidebar.contains(event.target) || menuToggle.contains(event.target)) return;
  closeMenu();
});

function activateSection(id) {
  navLinks.forEach((link) => {
    const active = link.hash === `#${id}`;
    link.classList.toggle("active", active);
    if (active) link.setAttribute("aria-current", "location");
    else link.removeAttribute("aria-current");
  });
  const section = sections.find((item) => item.id === id);
  if (section) mobileCurrent.textContent = section.dataset.title;
}

const sectionObserver = new IntersectionObserver(
  (entries) => {
    const visible = entries
      .filter((entry) => entry.isIntersecting)
      .sort((a, b) => b.intersectionRatio - a.intersectionRatio)[0];
    if (visible) activateSection(visible.target.id);
  },
  { rootMargin: "-15% 0px -70% 0px", threshold: [0, 0.1, 0.5] },
);

sections.forEach((section) => sectionObserver.observe(section));

function highlightSnippet(item, query) {
  const normalizedText = item.text.toLocaleLowerCase(locale);
  const index = normalizedText.indexOf(query.toLocaleLowerCase(locale));
  const start = Math.max(0, index === -1 ? 0 : index - 28);
  const end = Math.min(item.text.length, start + 100);
  return `${start > 0 ? "…" : ""}${item.text.slice(start, end)}${end < item.text.length ? "…" : ""}`;
}

function renderSearch(query = "") {
  const cleanQuery = query.trim();
  const matches = cleanQuery
    ? searchIndex.filter((item) => item.text.toLocaleLowerCase(locale).includes(cleanQuery.toLocaleLowerCase(locale)))
    : searchIndex.slice(0, 7);

  searchResults.replaceChildren();
  if (!matches.length) {
    const empty = document.createElement("p");
    empty.className = "search-empty";
    empty.textContent = uiStrings.searchEmptyTemplate.replace("{query}", cleanQuery);
    searchResults.append(empty);
    return;
  }

  matches.forEach((item) => {
    const link = document.createElement("a");
    link.className = "search-result";
    link.href = `#${item.id}`;
    const title = document.createElement("strong");
    title.textContent = item.title;
    const snippet = document.createElement("span");
    snippet.textContent = highlightSnippet(item, cleanQuery);
    link.append(title, snippet);
    link.addEventListener("click", () => searchDialog.close());
    searchResults.append(link);
  });
}

function openSearch() {
  renderSearch(searchInput.value);
  searchDialog.showModal();
  requestAnimationFrame(() => searchInput.focus());
}

searchTrigger.addEventListener("click", openSearch);
searchInput.addEventListener("input", () => renderSearch(searchInput.value));

searchDialog.addEventListener("click", (event) => {
  if (event.target === searchDialog) searchDialog.close();
});

document.addEventListener("keydown", (event) => {
  const typing = ["INPUT", "TEXTAREA"].includes(document.activeElement?.tagName);
  if (event.key === "/" && !typing && !searchDialog.open) {
    event.preventDefault();
    openSearch();
  }
  if (event.key === "Escape" && sidebar.classList.contains("open")) closeMenu();
});

let toastTimer;
function showToast(message) {
  toast.textContent = message;
  toast.classList.add("visible");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => toast.classList.remove("visible"), 1800);
}

async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    const input = document.createElement("textarea");
    input.value = text;
    input.style.position = "fixed";
    input.style.opacity = "0";
    body.append(input);
    input.select();
    document.execCommand("copy");
    input.remove();
  }
  showToast(uiStrings.copySuccess);
}

document.querySelectorAll("[data-copy]").forEach((button) => {
  button.addEventListener("click", () => copyText(button.dataset.copy));
});

function updateScrollTools() {
  backToTop.classList.toggle("visible", window.scrollY > 700);
}

window.addEventListener("scroll", updateScrollTools, { passive: true });
backToTop.addEventListener("click", () => window.scrollTo({ top: 0, behavior: "smooth" }));
updateScrollTools();

window.addEventListener("resize", () => {
  if (window.innerWidth > 760) closeMenu();
});
