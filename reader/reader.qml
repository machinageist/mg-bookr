// Author: Jeff
// Date: 2026-09-19
// Description: The reader window — one book, paged or scrolling, in the desktop's colours
// Notes: `library` is the one way out of this window (reader.py); it runs mg-bookr and nothing
//        else, so the window never touches the database or the book files. EPUB pages are drawn
//        by WebEngine with our stylesheet and helpers injected into an isolated world, so a
//        book's own scripts can never see or change them. PDFs come from QtQuick.Pdf and comics
//        are images. The place is saved on a debounce and again as the window closes.
//        Keys are Shortcuts rather than a key handler, because the web view holds the focus
//        while a page is open and would otherwise swallow them.
//        Rules.js holds everything that can be worked out without Qt, and node tests drive it

import QtQuick
import QtQuick.Window
import QtWebEngine
import QtQuick.Pdf
import "Rules.js" as Rules

Window {
    id: win

    width: 1100
    height: 860
    visible: true
    color: win.theme.base
    title: win.plan ? `${win.plan.book.title} — mg-bookr` : "mg-bookr"

    property var plan: null
    property var theme: Rules.palette("")
    property string kind: ""
    property string mode: "pages"
    property int fontSize: 18
    // which spine item, page or image we are on, and how far into it
    property int index: 0
    property real fraction: 0
    property string notice: ""
    // the EPUB view, once its component exists
    property var webView: null
    readonly property bool paged: win.mode === "pages"
    readonly property int items: win.kind === "epub" ? (win.plan ? win.plan.epub.spine.length : 1)
        : win.kind === "comic" ? (win.plan ? win.plan.pages.length : 1)
        : Math.max(1, pdf.pageCount)
    readonly property string chapter: win.kind === "epub" && win.plan
        ? Rules.chapterOf(win.plan.epub.toc, win.index) : ""
    readonly property real percent: Rules.percent(win.kind, win.index, win.fraction, win.items)

    // Read the theme, the book and how it was last read, then go where it was left
    Component.onCompleted: {
        win.theme = Rules.palette(library.theme());
        const answer = Rules.answer(library.plan());
        if (!answer) {
            win.notice = "mg-bookr could not open this book";
            return;
        }
        win.plan = answer;
        win.kind = answer.book.kind;
        const said = Rules.answer(library.mode());
        if (said && (said.mode === "pages" || said.mode === "scroll")) win.mode = said.mode;
        const at = win.kind === "epub"
            ? Rules.place(answer.book.location, answer.epub.spine.length)
            : { index: Rules.page(answer.book.location, win.items) - 1, fraction: 0 };
        win.index = at.index;
        win.fraction = at.fraction;
    }

    onClosing: win.save(true)

    // Tell mg-bookr where we are; `now` skips the debounce, for closing
    function save(now) {
        if (!win.plan) return;
        if (now) {
            saveSoon.stop();
            library.saveProgress(Rules.location(win.kind, win.index, win.fraction), win.percent);
        } else {
            saveSoon.restart();
        }
    }

    // Move a page or a screenful, and on to the next item when this one has run out
    function step(delta) {
        if (!win.plan) return;
        if (win.kind === "epub" && win.webView) {
            win.webView.ask(`__mgReader.advance(${delta})`, result => {
                if (result && result.moved) {
                    win.fraction = result.fraction;
                    win.save(false);
                } else {
                    win.turn(delta);
                }
            });
            return;
        }
        win.turn(delta);
    }

    // The next or previous item; a book ends where it ends
    function turn(delta) {
        const next = win.index + delta;
        if (next < 0 || next >= win.items) {
            win.say(delta > 0 ? "the end" : "the beginning");
            return;
        }
        win.index = next;
        // going back lands at the foot of the previous item, as turning a page does
        win.fraction = delta < 0 && win.kind === "epub" ? 1 : 0;
        win.save(false);
    }

    function setMode(mode) {
        win.mode = mode;
        library.setMode(mode);
        if (win.kind === "epub" && win.webView) win.webView.dress();
    }

    function setFont(size) {
        win.fontSize = Rules.clampFont(size);
        if (win.kind === "epub" && win.webView) win.webView.dress();
    }

    // Keep the selected words as a highlight; mg-bookr writes it into the vault note too
    function highlight() {
        if (win.kind !== "epub" || !win.webView) {
            win.say("highlights are for ebooks");
            return;
        }
        win.webView.ask("__mgReader.selection()", selection => {
            if (!selection) {
                win.say("select some words first");
                return;
            }
            const said = Rules.answer(library.addHighlight(
                Rules.location(win.kind, win.index, selection.fraction), selection.text, win.chapter));
            win.say(said ? "highlighted" : "the highlight was not saved");
        });
    }

    function say(words) {
        win.notice = words;
        clearNotice.restart();
    }

    Timer {
        id: saveSoon
        interval: 1200
        onTriggered: win.save(true)
    }

    Timer {
        id: clearNotice
        interval: 3000
        onTriggered: win.notice = ""
    }

    // ── keys ──

    Shortcut {
        sequences: ["Right", "PgDown", "Space"]
        onActivated: win.step(1)
    }
    Shortcut {
        sequences: ["Left", "PgUp"]
        onActivated: win.step(-1)
    }
    Shortcut {
        sequence: "T"
        onActivated: win.setMode(win.paged ? "scroll" : "pages")
    }
    Shortcut {
        sequences: ["+", "="]
        onActivated: win.setFont(win.fontSize + 1)
    }
    Shortcut {
        sequence: "-"
        onActivated: win.setFont(win.fontSize - 1)
    }
    Shortcut {
        sequence: "H"
        onActivated: win.highlight()
    }
    Shortcut {
        sequence: "N"
        onActivated: win.turn(1)
    }
    Shortcut {
        sequence: "P"
        onActivated: win.turn(-1)
    }
    Shortcut {
        sequences: ["Q", "Esc"]
        onActivated: win.close()
    }

    // ── the window ──

    Rectangle {
        id: header
        anchors { top: parent.top; left: parent.left; right: parent.right }
        height: 34
        color: win.theme.surface

        Rectangle {
            anchors { left: parent.left; right: parent.right; bottom: parent.bottom }
            height: 1
            color: win.theme.borderColor
        }

        Text {
            anchors { left: parent.left; leftMargin: 14; verticalCenter: parent.verticalCenter }
            width: parent.width - 220
            elide: Text.ElideRight
            color: win.theme.fg
            font.family: win.theme.fontFamily
            font.pixelSize: win.theme.fontSize
            text: win.plan
                ? win.plan.book.title + (win.chapter !== "" ? `  ·  ${win.chapter}` : "")
                : "…"
        }

        Text {
            anchors { right: parent.right; rightMargin: 14; verticalCenter: parent.verticalCenter }
            color: win.theme.faint
            font.family: win.theme.fontFamily
            font.pixelSize: win.theme.fontSize
            text: `${win.percent.toFixed(0)}%  ·  ${win.paged ? "pages" : "scroll"}`
        }
    }

    Loader {
        id: body
        anchors { top: header.bottom; left: parent.left; right: parent.right; bottom: footer.top }
        sourceComponent: win.kind === "epub" ? epubView
            : win.kind === "pdf" ? pdfView
            : win.kind === "comic" ? comicView
            : emptyView
    }

    Rectangle {
        id: footer
        anchors { bottom: parent.bottom; left: parent.left; right: parent.right }
        height: 26
        color: win.theme.surface

        Text {
            anchors { left: parent.left; leftMargin: 14; verticalCenter: parent.verticalCenter }
            width: parent.width - 28
            elide: Text.ElideRight
            color: win.notice !== "" ? win.theme.accent : win.theme.faint
            font.family: win.theme.fontFamily
            font.pixelSize: win.theme.fontSize - 1
            text: win.notice !== "" ? win.notice
                : "←/→ turn · t pages or scroll · +/- size · h highlight · n/p chapter · q close"
        }
    }

    // ── what each kind of book is drawn with ──

    Component {
        id: emptyView
        Item {}
    }

    Component {
        id: epubView

        WebEngineView {
            id: web
            anchors.fill: parent
            backgroundColor: win.theme.base
            url: win.plan ? "file://" + win.plan.epub.spine[win.index] : ""
            // a book is a document: it never opens windows or reaches the network
            settings.localContentCanAccessRemoteUrls: false
            settings.localContentCanAccessFileUrls: true
            settings.linksIncludedInFocusChain: false

            // Qt names this one LoadSucceededStatus; the short name is undefined and never matches
            onLoadingChanged: info => {
                if (info.status === WebEngineView.LoadSucceededStatus) web.dress();
            }
            onNewWindowRequested: request => request.action = WebEngineView.IgnoreRequest

            // Dress the page in our colours, install the helpers, then go to the saved place.
            // ApplicationWorld is a world of our own: the book's own scripts cannot see it
            function dress() {
                web.runJavaScript(Rules.pageScript(win.theme, win.mode, win.fontSize),
                    WebEngineScript.ApplicationWorld,
                    () => web.ask(`__mgReader.goto(${win.fraction})`, () => {}));
            }

            function ask(code, done) {
                web.runJavaScript(code, WebEngineScript.ApplicationWorld, done);
            }

            Component.onCompleted: win.webView = web
            Component.onDestruction: win.webView = null
        }
    }

    Component {
        id: pdfView

        PdfMultiPageView {
            id: pdfPages
            anchors.fill: parent
            document: pdf

            onCurrentPageChanged: {
                if (pdfPages.currentPage !== win.index) {
                    win.index = pdfPages.currentPage;
                    win.save(false);
                }
            }
            Component.onCompleted: pdfPages.goToPage(win.index)

            Connections {
                target: win
                function onIndexChanged() {
                    if (pdfPages.currentPage !== win.index) pdfPages.goToPage(win.index);
                }
            }
        }
    }

    Component {
        id: comicView

        Item {
            Image {
                anchors.fill: parent
                visible: win.paged
                fillMode: Image.PreserveAspectFit
                asynchronous: true
                source: win.plan && win.paged ? "file://" + win.plan.pages[win.index] : ""
            }

            ListView {
                id: strip
                anchors.fill: parent
                visible: !win.paged
                model: win.paged || !win.plan ? [] : win.plan.pages
                cacheBuffer: win.height * 2
                delegate: Image {
                    required property string modelData
                    width: strip.width
                    fillMode: Image.PreserveAspectFit
                    asynchronous: true
                    source: "file://" + modelData
                }
                // the page in the middle of the view is the one being read
                onContentYChanged: {
                    const at = strip.indexAt(strip.width / 2, strip.contentY + strip.height / 2);
                    if (at >= 0 && at !== win.index) {
                        win.index = at;
                        win.save(false);
                    }
                }
                Component.onCompleted: strip.positionViewAtIndex(win.index, ListView.Beginning)
            }
        }
    }

    PdfDocument {
        id: pdf
        source: win.plan && win.kind === "pdf" ? "file://" + win.plan.file : ""
    }
}
