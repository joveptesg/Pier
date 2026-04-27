// Convert ANSI SGR escape sequences (color codes) into HTML <span>s.
// Used by the container/build log viewers so that pretty-loggers
// (pino-pretty, tracing pretty, rich, zerolog console) render with color
// instead of leaking literal `[32m`/`[39m` bytes into the page.
(function () {
    var FG = {
        30: '#6e7681', 31: '#f87171', 32: '#4ade80', 33: '#facc15',
        34: '#60a5fa', 35: '#c084fc', 36: '#22d3ee', 37: '#f4f4f5',
        90: '#9ca3af', 91: '#fca5a5', 92: '#86efac', 93: '#fde047',
        94: '#93c5fd', 95: '#d8b4fe', 96: '#67e8f9', 97: '#ffffff'
    };

    function escapeHtml(s) {
        return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
    }

    window.escapeHtml = window.escapeHtml || escapeHtml;

    window.stripAnsi = function (s) {
        return (s || '').replace(/\x1b\[[\d;]*m/g, '');
    };

    window.ansiToHtml = function (raw) {
        if (!raw) return '';
        var out = '', open = 0, i = 0;
        var re = /\x1b\[([\d;]*)m/g;
        var m;
        while ((m = re.exec(raw)) !== null) {
            out += escapeHtml(raw.slice(i, m.index));
            i = m.index + m[0].length;
            var codes = (m[1] || '0').split(';').map(Number);
            for (var k = 0; k < codes.length; k++) {
                var c = codes[k];
                if (c === 0 || c === 39) {
                    while (open > 0) { out += '</span>'; open--; }
                } else if (c === 1) {
                    out += '<span style="font-weight:600">'; open++;
                } else if (FG[c]) {
                    out += '<span style="color:' + FG[c] + '">'; open++;
                }
            }
        }
        out += escapeHtml(raw.slice(i));
        while (open > 0) { out += '</span>'; open--; }
        return out;
    };
})();
