// Language switcher for the mdBook-i18n-helpers setup (see docs-site/README.md).
//
// mdBook itself has no notion of multiple languages -- each `MDBOOK_BOOK__LANGUAGE=<lang>
// mdbook build` invocation produces an independent, self-contained site under its own
// output directory (this project publishes them side by side as book/en/ and
// book/pt-BR/, see .github/workflows/docs.yml). This script just adds a button to the
// menu bar that swaps the language segment of the current URL, so a reader lands on the
// same page in the other language instead of that book's home page.
(function () {
  var LANGUAGES = { en: "EN", "pt-BR": "PT-BR" };

  function otherLanguage(current) {
    return Object.keys(LANGUAGES).find(function (lang) {
      return lang !== current;
    });
  }

  function currentLanguageFromPath(pathname) {
    for (var lang in LANGUAGES) {
      if (pathname.indexOf("/" + lang + "/") !== -1) {
        return lang;
      }
    }
    return null;
  }

  function init() {
    var rightButtons = document.querySelector(".right-buttons");
    if (!rightButtons) return;

    var current = currentLanguageFromPath(window.location.pathname);
    // No /en/ or /pt-BR/ segment in the URL -- e.g. a plain `mdbook serve` preview of a
    // single language build with no sibling translation deployed next to it. There is
    // nothing to switch to, so skip adding the button rather than link somewhere broken.
    if (!current) return;

    var target = otherLanguage(current);
    var targetPath = window.location.pathname.replace(
      "/" + current + "/",
      "/" + target + "/"
    );

    var link = document.createElement("a");
    link.id = "lang-switch-button";
    link.className = "icon-button";
    link.href = targetPath + window.location.search + window.location.hash;
    link.title = "Switch to " + LANGUAGES[target];
    link.setAttribute("aria-label", "Switch to " + LANGUAGES[target]);
    // A plain text label ("EN" / "PT-BR"), not an icon -- the other menu-bar buttons
    // are icon-only, but there is no universally-understood glyph for "language" the
    // way there is for print or search, and the text doubles as its own tooltip.
    link.textContent = LANGUAGES[target];
    link.classList.add("lang-switch-button");

    rightButtons.insertBefore(link, rightButtons.firstChild);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
