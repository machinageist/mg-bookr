// Author: Jeff
// Date: 2026-09-19
// Description: The reader window's rules — where you are in a book, and the code injected into a page
// Notes: No Qt here, so node tests can drive it: reader/tests/rules.test.js. Locations match the
//        store: EPUB "spine:fraction", PDF and comics "page". The injected script is built as one
//        string and runs in WebEngine's isolated world, so a book's own scripts can never see it.
//        Top-level var and function declarations only, because the tests load this into a vm context

var MIN_FONT = 10;
var MAX_FONT = 32;
var PADDING = 48;
var MAX_QUOTE = 2000;
// the longest comfortable line of text
var MEASURE = "34em";

// "3:0.42" → { index: 3, fraction: 0.42 }; anything else starts at the beginning
function place(location, items) {
    var at = { index: 0, fraction: 0 };
    var parts = String(location || "").split(":");
    var index = parseInt(parts[0], 10);
    if (isFinite(index) && index >= 0) at.index = Math.min(index, Math.max(0, items - 1));
    var fraction = parseFloat(parts[1]);
    if (isFinite(fraction) && fraction > 0) at.fraction = Math.min(1, fraction);
    return at;
}

// A page number location for PDFs and comics ("7" is the seventh page, counted from 1)
function page(location, pages) {
    var n = parseInt(String(location || "").split(":")[0], 10);
    if (!isFinite(n) || n < 1) return 1;
    return Math.min(n, Math.max(1, pages));
}

// Where we are, as the store keeps it
function location(kind, index, fraction) {
    return kind === "epub" ? index + ":" + fraction.toFixed(4) : String(index);
}

// How far through the whole book, as a percentage
function percent(kind, index, fraction, items) {
    if (items <= 0) return 0;
    var done = kind === "epub" ? index + fraction : index;
    var whole = kind === "epub" ? items : items;
    return Math.max(0, Math.min(100, done / whole * 100));
}

// The chapter a spine item belongs to: the last entry at or before it
function chapterOf(toc, index) {
    var title = "";
    for (var i = 0; i < (toc || []).length; i++) {
        if (typeof toc[i].spine === "number" && toc[i].spine <= index) title = toc[i].title;
    }
    return title;
}

// Keep the reading size sensible
function clampFont(size) {
    return Math.max(MIN_FONT, Math.min(MAX_FONT, Math.round(size)));
}

// The stylesheet a page is dressed in: the desktop's colours, one comfortable measure, and
// (in pages mode) columns the width of the window, which is what makes paging work
function css(theme, mode, fontSize) {
    var pad = PADDING;
    // The side margin grows on a wide window so a line never runs past a comfortable measure.
    // In pages mode the gap is twice that margin, which makes one column exactly one screenful:
    // column + gap = measure + (viewport - measure) = viewport, so paging can scroll by the
    // window's width and always land on a column edge
    var side = "max(" + pad + "px, calc((100vw - " + MEASURE + ") / 2))";
    var columns = mode === "pages"
        ? "html { height: 100vh !important; overflow: hidden !important; }\n" +
          "body { height: calc(100vh - " + (pad * 2) + "px) !important; " +
          "column-width: calc(100vw - 2 * " + side + ") !important; " +
          "column-gap: calc(2 * " + side + ") !important; }\n"
        : "html, body { height: auto !important; overflow-x: hidden !important; }\n";
    return "html, body { background: " + theme.base + " !important; color: " + theme.fg + " !important; }\n" +
        "body { margin: 0 !important; padding: " + pad + "px " + side + " !important; font-family: " + theme.bookFont +
        "; font-size: " + clampFont(fontSize) + "px !important; line-height: 1.65 !important; " +
        "text-align: left !important; hyphens: auto; }\n" +
        "p, li, div, span, td { color: " + theme.fg + " !important; }\n" +
        "h1, h2, h3, h4, h5, h6 { color: " + theme.fg + " !important; line-height: 1.25; }\n" +
        "a { color: " + theme.accent + " !important; }\n" +
        "img, svg, video { max-width: 100% !important; height: auto !important; }\n" +
        "code, pre { background: " + theme.surface + " !important; color: " + theme.green + " !important; }\n" +
        "blockquote { border-left: 2px solid " + theme.accent + " !important; margin-left: 0; padding-left: 1em; }\n" +
        "hr { border-color: " + theme.borderColor + " !important; }\n" +
        "::selection { background: " + theme.accent + "; color: " + theme.base + "; }\n" +
        columns;
}

// The whole script injected into a page: it dresses the page, then answers the window's
// questions about where we are, what is selected, and where to go next
function pageScript(theme, mode, fontSize) {
    var options = JSON.stringify({ css: css(theme, mode, fontSize), mode: mode, maxQuote: MAX_QUOTE });
    return "(function () {\n" +
        "  var o = " + options + ";\n" +
        "  var style = document.getElementById('mg-reader-style') || document.createElement('style');\n" +
        "  style.id = 'mg-reader-style';\n" +
        "  style.textContent = o.css;\n" +
        "  (document.head || document.documentElement).appendChild(style);\n" +
        "  var pages = o.mode === 'pages';\n" +
        "  function span() {\n" +
        "    return pages ? Math.max(1, document.body.scrollWidth - window.innerWidth)\n" +
        "                 : Math.max(1, document.body.scrollHeight - window.innerHeight);\n" +
        "  }\n" +
        "  function at() { return pages ? window.scrollX : window.scrollY; }\n" +
        "  window.__mgReader = {\n" +
        "    progress: function () { return Math.min(1, Math.max(0, at() / span())); },\n" +
        "    ended: function () { return at() >= span() - 2; },\n" +
        "    started: function () { return at() <= 2; },\n" +
        // a fraction of the whole item, so a resize or a font change lands in the same place
        "    goto: function (fraction) {\n" +
        "      var to = Math.round(span() * Math.min(1, Math.max(0, fraction)));\n" +
        "      if (pages) { to = Math.round(to / window.innerWidth) * window.innerWidth; window.scrollTo(to, 0); }\n" +
        "      else window.scrollTo(0, to);\n" +
        "      return this.progress();\n" +
        "    },\n" +
        "    step: function (delta) {\n" +
        "      var by = pages ? window.innerWidth * delta : Math.round(window.innerHeight * 0.9) * delta;\n" +
        "      if (pages) window.scrollTo(window.scrollX + by, 0); else window.scrollBy(0, by);\n" +
        "      return this.progress();\n" +
        "    },\n" +
        // a page turn that says whether it actually moved, so the window knows when an item ran out
        "    advance: function (delta) {\n" +
        "      var before = at();\n" +
        "      this.step(delta);\n" +
        "      return { moved: at() !== before, fraction: this.progress() };\n" +
        "    },\n" +
        "    selection: function () {\n" +
        "      var s = window.getSelection();\n" +
        "      if (!s || s.isCollapsed) return null;\n" +
        "      var text = s.toString().replace(/\\s+/g, ' ').trim().slice(0, o.maxQuote);\n" +
        "      return text === '' ? null : { text: text, fraction: this.progress() };\n" +
        "    }\n" +
        "  };\n" +
        "  return true;\n" +
        "})()";
}

// What `mg-bookr --json <anything>` said, or null when it could not be read
function answer(output) {
    var value;
    try { value = JSON.parse(output); } catch (e) { return null; }
    return value && value.ok !== false ? value : null;
}

// The palette from the shell, filled in wherever it is silent
function palette(output) {
    var theme = answer(output) || {};
    var fallback = {
        name: "dracula", dark: true, base: "#282a36", surface: "#21222c", fg: "#f8f8f2",
        muted: "#6272a4", faint: "#8f96ad", borderColor: "#44475a", accent: "#bd93f9",
        red: "#ff5555", orange: "#ffb86c", yellow: "#f1fa8c", green: "#50fa7b",
        cyan: "#8be9fd", blue: "#6272a4", purple: "#bd93f9", pink: "#ff79c6",
        fontFamily: "sans-serif", fontSize: 13, radius: 0
    };
    var out = {};
    for (var key in fallback) out[key] = typeof theme[key] === typeof fallback[key] ? theme[key] : fallback[key];
    // the desktop font is for the window's own chrome; a book is read in a serif face
    out.bookFont = "Georgia, 'Times New Roman', serif";
    return out;
}
