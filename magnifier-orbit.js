/**
 * ASCII Magnifying Glass — Trail Orbit
 *
 * Renders a Mac-style folder with a magnifying glass orbiting around it,
 * leaving a fading trail of dots behind it.
 *
 * Usage (one-shot, runs until Ctrl+C):
 *   node magnifier-orbit.js
 *
 * Usage (programmatic):
 *   const { MagnifierOrbit } = require('./magnifier-orbit');
 *   const anim = new MagnifierOrbit({ frameMs: 80 });
 *   anim.start();
 *   // ...later...
 *   anim.stop();
 *
 * Or render a single frame to a string (no terminal side effects):
 *   const { renderFrame } = require('./magnifier-orbit');
 *   console.log(renderFrame(42));
 */

'use strict';

// ---------------------------------------------------------------------------
// Grid helpers
// ---------------------------------------------------------------------------

function makeGrid(rows, cols, fill = ' ') {
  return Array.from({ length: rows }, () => Array(cols).fill(fill));
}

function gridToString(grid) {
  return grid.map((r) => r.join('')).join('\n');
}

function placeText(grid, row, col, text) {
  for (let i = 0; i < text.length; i++) {
    if (grid[row] && col + i >= 0 && col + i < grid[row].length) {
      grid[row][col + i] = text[i];
    }
  }
}

function setCell(g, r, c, ch) {
  if (g[r] && c >= 0 && c < g[r].length) g[r][c] = ch;
}

// ---------------------------------------------------------------------------
// Mac folder drawing (medium size — 6 rows tall, 18 wide)
// ---------------------------------------------------------------------------

function drawMacFolder(g, cy, cx) {
  placeText(g, cy - 3, cx - 8, '   ______');
  placeText(g, cy - 2, cx - 8, '  /      \\________');
  placeText(g, cy - 1, cx - 8, ' /                \\');
  placeText(g, cy,     cx - 8, '|                  |');
  placeText(g, cy + 1, cx - 8, '|                  |');
  placeText(g, cy + 2, cx - 8, '|__________________|');
}

// ---------------------------------------------------------------------------
// Orbit math
// ---------------------------------------------------------------------------

const ROWS = 14;
const COLS = 44;
const CY = 7;     // folder center Y
const CX = 22;    // folder center X
const RX = 16;    // orbit radius X
const RY = 5;     // orbit radius Y (smaller because chars are ~2x tall)
const ANGULAR_SPEED = 0.16; // radians per frame
const TRAIL_LEN = 6;
const TRAIL_CHARS = ['.', '.', ':', ':', 'o', 'o']; // oldest -> newest

function posAt(frame) {
  const angle = (frame * ANGULAR_SPEED) % (Math.PI * 2);
  return {
    mx: CX + Math.round(Math.cos(angle) * RX),
    my: CY + Math.round(Math.sin(angle) * RY),
    angle,
  };
}

// Don't draw trail dots that would overlap the folder
function isInsideFolder(x, y) {
  return x > CX - 9 && x < CX + 9 && y > CY - 4 && y < CY + 3;
}

// ---------------------------------------------------------------------------
// Frame renderer
// ---------------------------------------------------------------------------

function renderFrame(frame) {
  const g = makeGrid(ROWS, COLS);
  drawMacFolder(g, CY, CX);

  // Draw fading trail (oldest first so newest overwrites)
  for (let i = TRAIL_LEN; i >= 1; i--) {
    const p = posAt(frame - i);
    if (isInsideFolder(p.mx, p.my)) continue;
    setCell(g, p.my, p.mx, TRAIL_CHARS[TRAIL_LEN - i]);
  }

  // Current magnifier glass
  const cur = posAt(frame);
  placeText(g, cur.my, cur.mx - 1, '(O)');

  // Handle trails behind orbit direction
  const handleSide = Math.cos(cur.angle) > 0 ? 2 : -2;
  const handleChar = Math.cos(cur.angle) > 0 ? '\\' : '/';
  setCell(g, cur.my + 1, cur.mx + handleSide, handleChar);

  return gridToString(g);
}

// ---------------------------------------------------------------------------
// Terminal animation controller
// ---------------------------------------------------------------------------

const HIDE_CURSOR = '\x1b[?25l';
const SHOW_CURSOR = '\x1b[?25h';
const CLEAR_SCREEN = '\x1b[2J';
const MOVE_HOME = '\x1b[H';

class MagnifierOrbit {
  constructor(opts = {}) {
    this.frameMs = opts.frameMs || 80;
    this.stream = opts.stream || process.stdout;
    this.frame = 0;
    this.timer = null;
    this._exitHandler = null;
  }

  start() {
    if (this.timer) return;

    this.stream.write(HIDE_CURSOR + CLEAR_SCREEN + MOVE_HOME);

    // Make sure cursor is restored on exit / Ctrl+C
    this._exitHandler = () => this.stop();
    process.once('SIGINT', () => { this.stop(); process.exit(0); });
    process.once('exit', this._exitHandler);

    this.timer = setInterval(() => {
      this.stream.write(MOVE_HOME + renderFrame(this.frame));
      this.frame++;
    }, this.frameMs);
  }

  stop() {
    if (this.timer) {
      clearInterval(this.timer);
      this.timer = null;
    }
    this.stream.write('\n' + SHOW_CURSOR);
    if (this._exitHandler) {
      process.removeListener('exit', this._exitHandler);
      this._exitHandler = null;
    }
  }
}

// ---------------------------------------------------------------------------
// Exports + CLI entrypoint
// ---------------------------------------------------------------------------

module.exports = { MagnifierOrbit, renderFrame };

if (require.main === module) {
  const anim = new MagnifierOrbit();
  anim.start();
}
