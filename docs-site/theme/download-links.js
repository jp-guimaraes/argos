// Populates #latest-release-downloads (src/download.md) with real links to the
// current release's assets, fetched live from the GitHub API -- so this page
// never goes stale the way a hand-typed filename/version would on every
// release. See docs-site/README.md for why src/*.md otherwise never contains
// content the repo doesn't already have elsewhere; this is the one page whose
// whole point is data GitHub itself is the source of truth for.
(function () {
  var REPO = "jp-guimaraes/argos";

  // Same detection lang-switcher.js uses -- keeps this script's own small
  // amount of UI text (not markdown, so gettext never sees it) in step with
  // whichever of the two published language trees the reader is on.
  function currentLanguage() {
    return window.location.pathname.indexOf("/pt-BR/") !== -1 ? "pt-BR" : "en";
  }

  var STRINGS = {
    en: {
      loading: "Loading the latest release from GitHub…",
      error:
        "Could not reach the GitHub API just now. Get the files directly from the ",
      errorLinkText: "latest release page",
      version: "Version",
      totalDownloads: "Total downloads, all releases combined",
      downloadsSuffix: function (n) {
        return n === 1 ? "download" : "downloads";
      },
      categories: {
        dmg: "macOS -- universal .dmg (Apple Silicon + Intel)",
        deb: "Linux -- .deb (Debian, Ubuntu)",
        linuxTarball: "Linux -- tarball (x86_64)",
        macosArm: "macOS -- tarball (Apple Silicon)",
        macosIntel: "macOS -- tarball (Intel)",
        other: "Other files",
      },
    },
    "pt-BR": {
      loading: "Carregando o último release do GitHub…",
      error: "Não foi possível acessar a API do GitHub agora. Baixe direto da ",
      errorLinkText: "página do último release",
      version: "Versão",
      totalDownloads: "Total de downloads, somando todos os releases",
      downloadsSuffix: function (n) {
        return n === 1 ? "download" : "downloads";
      },
      categories: {
        dmg: "macOS -- .dmg universal (Apple Silicon + Intel)",
        deb: "Linux -- .deb (Debian, Ubuntu)",
        linuxTarball: "Linux -- tarball (x86_64)",
        macosArm: "macOS -- tarball (Apple Silicon)",
        macosIntel: "macOS -- tarball (Intel)",
        other: "Outros arquivos",
      },
    },
  };

  // Matches this repo's actual release.yml asset names (checked against a
  // real release, not guessed): Argos-<ver>.dmg, argos_<ver>-1_amd64.deb,
  // argos-<tag>-x86_64-unknown-linux-gnu.tar.gz, ...-aarch64-apple-darwin...,
  // ...-x86_64-apple-darwin.tar.gz.
  function categoryFor(name) {
    if (/\.dmg$/i.test(name)) return "dmg";
    if (/\.deb$/i.test(name)) return "deb";
    if (/aarch64-apple-darwin/.test(name)) return "macosArm";
    if (/x86_64-apple-darwin/.test(name)) return "macosIntel";
    if (/x86_64-unknown-linux-gnu/.test(name)) return "linuxTarball";
    return "other";
  }

  var ORDER = ["dmg", "deb", "linuxTarball", "macosArm", "macosIntel", "other"];

  function humanSize(bytes) {
    var mib = bytes / (1024 * 1024);
    return mib.toFixed(1) + " MiB";
  }

  // GitHub tracks this per asset natively (release.assets[].download_count) --
  // no separate counter service needed. Formatted per-locale (1,234 vs
  // 1.234) with the current page's language as the locale tag.
  function humanCount(n, lang, strings) {
    return n.toLocaleString(lang) + " " + strings.downloadsSuffix(n);
  }

  // `releases` is every release, newest first (GitHub's own order) -- assets
  // and their per-file download_count come from releases[0], the total sums
  // every release's assets so an old tag's downloads are never lost from the
  // count just because a newer version shipped.
  function render(container, strings, lang, releases) {
    container.textContent = "";
    var latest = releases[0];

    var version = document.createElement("p");
    var versionLabel = document.createElement("strong");
    versionLabel.textContent = strings.version + ": ";
    version.appendChild(versionLabel);
    version.appendChild(document.createTextNode(latest.tag_name));
    container.appendChild(version);

    var totalDownloads = releases.reduce(function (sum, release) {
      return (
        sum +
        release.assets.reduce(function (s, asset) {
          return s + asset.download_count;
        }, 0)
      );
    }, 0);
    var total = document.createElement("p");
    var totalLabel = document.createElement("strong");
    totalLabel.textContent = strings.totalDownloads + ": ";
    total.appendChild(totalLabel);
    total.appendChild(document.createTextNode(totalDownloads.toLocaleString(lang)));
    container.appendChild(total);

    var byCategory = {};
    latest.assets.forEach(function (asset) {
      var cat = categoryFor(asset.name);
      (byCategory[cat] = byCategory[cat] || []).push(asset);
    });

    var list = document.createElement("ul");
    list.className = "download-asset-list";
    ORDER.forEach(function (cat) {
      var assets = byCategory[cat];
      if (!assets) return;
      assets.forEach(function (asset) {
        var item = document.createElement("li");
        var link = document.createElement("a");
        link.href = asset.browser_download_url;
        link.textContent = strings.categories[cat];
        item.appendChild(link);
        item.appendChild(
          document.createTextNode(
            " -- " +
              asset.name +
              " (" +
              humanSize(asset.size) +
              ", " +
              humanCount(asset.download_count, lang, strings) +
              ")"
          )
        );
        list.appendChild(item);
      });
    });
    container.appendChild(list);
  }

  function renderError(container, strings) {
    container.textContent = "";
    var p = document.createElement("p");
    p.appendChild(document.createTextNode(strings.error));
    var link = document.createElement("a");
    link.href = "https://github.com/" + REPO + "/releases/latest";
    link.textContent = strings.errorLinkText;
    p.appendChild(link);
    p.appendChild(document.createTextNode("."));
    container.appendChild(p);
  }

  function init() {
    var container = document.getElementById("latest-release-downloads");
    if (!container) return; // not on the download page

    var lang = currentLanguage();
    var strings = STRINGS[lang];
    container.textContent = strings.loading;

    // The list endpoint, not /releases/latest -- one call gives both the
    // latest release (releases[0], same one /latest would return) and every
    // other release's assets, which the total-downloads count needs. This
    // repo has 8 releases, well under the 30-per-page default, so no
    // pagination is needed to have every one of them.
    fetch("https://api.github.com/repos/" + REPO + "/releases")
      .then(function (res) {
        if (!res.ok) throw new Error("GitHub API responded " + res.status);
        return res.json();
      })
      .then(function (releases) {
        if (!releases.length) throw new Error("no releases returned");
        render(container, strings, lang, releases);
      })
      .catch(function () {
        renderError(container, strings);
      });
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
