// E2E: real browser against `imagecropper album --serve`, verifying the grid
// renders (colors included), selection works, zoom works, delete works.

const { test, expect } = require("@playwright/test");
const fs = require("fs");
const path = require("path");
const { EXPECTED } = require("../fixtures");

const FIXTURE_DIR = path.join(__dirname, "..", ".fixtures");
const MOVE_DIR = path.join(__dirname, "..", ".move-target");
const N_IMAGES = Object.keys(EXPECTED).length;

// Fixture names sorted the way the manifest orders them (filename).
const SORTED = Object.keys(EXPECTED).sort();

async function openApp(page) {
  await page.goto("/");
  await expect(page.locator("#c")).toBeVisible();
}

async function waitAllCached(page) {
  // The shared server's manifest may shrink after delete/move tests, and
  // tombstoned items keep their slot — count only live tiles.
  await page.waitForFunction(
    () => {
      const live = items.filter((it) => it && !it.removed).length;
      return live > 0 && thumbs.size >= live;
    },
    null,
    { timeout: 30_000 },
  );
}

const tileRects = (page) =>
  page.evaluate(() => tileRects().map(({ i, x, y, w, h }) => ({ i, x, y, w, h })));

const pixelAt = (page, x, y) =>
  page.evaluate(
    ([px, py]) =>
      Array.from(
        document.getElementById("c").getContext("2d").getImageData(px, py, 1, 1).data,
      ),
    [Math.round(x), Math.round(y)],
  );

const colorClose = (actual, expected, tol = 60) =>
  expected.every((v, i) => Math.abs(actual[i] - v) <= tol);

test.describe("album grid", () => {
  test("loads manifest and renders every coloured tile", async ({ page }) => {
    await openApp(page);
    await waitAllCached(page);
    await expect(page.locator("#top")).toContainText(`${N_IMAGES} images`);

    const rects = await tileRects(page);
    expect(rects.length).toBe(N_IMAGES);

    for (const rect of rects) {
      const name = SORTED[rect.i];
      const px = await pixelAt(page, rect.x + rect.w / 2, rect.y + rect.h / 2);
      expect(
        colorClose(px, EXPECTED[name]),
        `tile ${name}: expected ~${EXPECTED[name]}, got ${px}`,
      ).toBeTruthy();
    }
  });

  test("purple and red tiles are distinguishable", async ({ page }) => {
    await openApp(page);
    await waitAllCached(page);
    const rects = await tileRects(page);
    const purpleIdx = SORTED.indexOf("purple");
    const redIdx = SORTED.indexOf("red");
    const p = rects[purpleIdx], r = rects[redIdx];
    const pp = await pixelAt(page, p.x + p.w / 2, p.y + p.h / 2);
    const rp = await pixelAt(page, r.x + r.w / 2, r.y + r.h / 2);
    expect(pp[0]).toBeGreaterThan(100); // purple: strong red+blue
    expect(pp[2]).toBeGreaterThan(120);
    expect(pp[1]).toBeLessThan(90);    // but weak green
    expect(rp[0]).toBeGreaterThan(150); // red: dominant red only
    expect(rp[1]).toBeLessThan(90);
    expect(rp[2]).toBeLessThan(90);
  });

  test("click / ctrl+click / shift+click selection", async ({ page }) => {
    await openApp(page);
    await waitAllCached(page);
    const rects = await tileRects(page);
    const center = (r) => ({ x: r.x + r.w / 2, y: r.y + r.h / 2 });

    // Plain click selects exactly one.
    await page.mouse.click(center(rects[0]).x, center(rects[0]).y);
    await expect(page.locator("#top")).toContainText("1 selected");

    // Ctrl+click adds another.
    await page.keyboard.down("Control");
    await page.mouse.click(center(rects[1]).x, center(rects[1]).y);
    await page.keyboard.up("Control");
    await expect(page.locator("#top")).toContainText("2 selected");
    // The selection's byte size is shown next to the count.
    await expect(page.locator("#top")).toContainText(
      /2 selected \(\d+(\.\d+)? [KMGT]?B\)/,
    );

    // Shift+click extends to a range ending at the anchor (rects[1]=anchor).
    await page.keyboard.down("Shift");
    await page.mouse.click(center(rects[4]).x, center(rects[4]).y);
    await page.keyboard.up("Shift");
    await expect(page.locator("#top")).toContainText("5 selected");
  });

  test("arrow keys move selection; space toggles", async ({ page }) => {
    await openApp(page);
    await waitAllCached(page);
    const rects = await tileRects(page);
    await page.mouse.click(rects[0].x + rects[0].w / 2, rects[0].y + rects[0].h / 2);

    await page.keyboard.press("ArrowRight");
    let sel = await page.evaluate(() => [...selected]);
    expect(sel).toEqual([1]);

    // Space toggles off — 0 selected.
    await page.keyboard.press(" ");
    sel = await page.evaluate(() => [...selected]);
    expect(sel.length).toBe(0);

    // Ctrl+A selects all.
    await page.keyboard.press("Control+a");
    await expect(page.locator("#top")).toContainText(`${N_IMAGES} selected`);
  });

  test("mouse drag selects the contiguous sequence (default mode)", async ({
    page,
  }) => {
    await openApp(page);
    await waitAllCached(page);
    const rects = await tileRects(page);
    const r0 = rects[0];
    // First tile of the row below — the default sequence mode must select
    // the full contiguous run r0.i..row1First.i even though the band only
    // crosses the first column.
    const next = rects.find((r) => r.y > r0.y + r0.h - 1);
    expect(next, "expected at least two rows").toBeTruthy();

    await page.mouse.move(r0.x + r0.w * 0.2, r0.y + r0.h * 0.5);
    await page.mouse.down();
    await page.mouse.move(next.x + next.w * 0.2, next.y + next.h * 0.5, {
      steps: 8,
    });
    await page.mouse.up();

    const sel = await page.evaluate(() => [...selected].sort((a, b) => a - b));
    const want = [];
    for (let i = r0.i; i <= next.i; i++) want.push(i);
    expect(sel).toEqual(want);
  });

  test("rubber-band mode selects only intersected tiles", async ({ page }) => {
    // Separate server on :8791 with --mouse-select-mode=rubber-band.
    await page.goto("http://127.0.0.1:8791/");
    await expect(page.locator("#c")).toBeVisible();
    await waitAllCached(page);
    const rects = await tileRects(page);
    const r0 = rects[0];
    const next = rects.find((r) => r.y > r0.y + r0.h - 1);
    expect(next, "expected at least two rows").toBeTruthy();

    // Narrow vertical band inside the first column — rubber-band picks up
    // only the two tiles it actually crosses (sequence would take the
    // whole contiguous run r0.i..next.i).
    await page.mouse.move(r0.x + r0.w * 0.2, r0.y + r0.h * 0.5);
    await page.mouse.down();
    await page.mouse.move(next.x + r0.w * 0.2, next.y + next.h * 0.5, {
      steps: 8,
    });
    await page.mouse.up();

    const sel = await page.evaluate(() => [...selected].sort((a, b) => a - b));
    expect(sel).toEqual([r0.i, next.i]);
  });

  test("+/- zoom and w/W restrict the viewer width", async ({ page }) => {
    await openApp(page);
    await waitAllCached(page);
    const before = await tileRects(page);

    await page.keyboard.press("+");
    const zoomed = await tileRects(page);
    expect(zoomed[0].w).toBeGreaterThan(before[0].w * 1.05);

    await page.keyboard.press("-");
    const unzoomed = await tileRects(page);
    expect(unzoomed[0].w).toBeLessThan(zoomed[0].w);

    // w restricts the viewer: the grid narrows into a centered band and
    // shrinks further with every press; W (Shift+w) grows it back.
    const span = (rs) =>
      Math.max(...rs.map((r) => r.x + r.w)) - Math.min(...rs.map((r) => r.x));
    await page.keyboard.press("w");
    const narrow = await tileRects(page);
    expect(span(narrow)).toBeLessThan(span(unzoomed) * 0.9);
    expect(Math.min(...narrow.map((r) => r.x))).toBeGreaterThan(8); // margin

    await page.keyboard.press("w");
    const narrower = await tileRects(page);
    expect(span(narrower)).toBeLessThan(span(narrow) * 0.9);

    await page.keyboard.press("Shift+w");
    await page.keyboard.press("Shift+w");
    const restored = await tileRects(page);
    expect(span(restored)).toBeGreaterThan(span(narrower) * 1.2);
  });

  test("f toggles the focused image fullscreen", async ({ page }) => {
    await openApp(page);
    await waitAllCached(page);

    // Focus the purple tile and press f — the ≤1920 preview is fetched
    // over /bulk (fmt=2) and drawn fullscreen until toggled off.
    const purpleIdx = await page.evaluate(
      () => items.findIndex((it) => it.name === "purple.png"),
    );
    await page.evaluate((i) => {
      focus = i;
      draw();
    }, purpleIdx);

    await page.keyboard.press("f");
    await page.waitForFunction(() => fsMode && screenBmp, null, {
      timeout: 15_000,
    });

    // Canvas centre must show the purple preview, not the grid.
    const [cx, cy] = await page.evaluate(() => [
      (innerWidth * devicePixelRatio) / 2,
      (innerHeight * devicePixelRatio) / 2,
    ]);
    const px = await pixelAt(page, cx, cy);
    expect(colorClose(px, EXPECTED["purple"]), `got ${px}`).toBeTruthy();

    // Pressing f again restores the grid.
    await page.keyboard.press("f");
    await expect(page.locator("#top")).toContainText("images");
    await page.waitForFunction(() => !fsMode);
  });

  test("f picks the hovered tile once; arrows navigate; hover ignored", async ({
    page,
  }) => {
    await openApp(page);
    await waitAllCached(page);
    const rects = await tileRects(page);
    const redIdx = SORTED.indexOf("red");
    const purpleIdx = SORTED.indexOf("purple");
    const yellowIdx = SORTED.indexOf("yellow");

    // Focus is purple while the mouse parks over red — entering f mode
    // shows the hovered tile.
    await page.evaluate((i) => {
      focus = i;
      draw();
    }, purpleIdx);
    const r = rects.find((t) => t.i === redIdx);
    await page.mouse.move(r.x + r.w / 2, r.y + r.h / 2);

    await page.keyboard.press("f");
    await page.waitForFunction(
      (i) => fsMode && screenIdx === i && screenBmp,
      redIdx,
      { timeout: 15_000 },
    );
    const centre = await page.evaluate(() => [
      (innerWidth * devicePixelRatio) / 2,
      (innerHeight * devicePixelRatio) / 2,
    ]);
    let px = await pixelAt(page, centre[0], centre[1]);
    expect(colorClose(px, EXPECTED["red"]), `got ${px}`).toBeTruthy();

    // Moving the mouse over a different tile must NOT switch the preview.
    const p = rects.find((t) => t.i === purpleIdx);
    await page.mouse.move(p.x + p.w / 2, p.y + p.h / 2);
    await page.waitForTimeout(300);
    px = await pixelAt(page, centre[0], centre[1]);
    expect(
      colorClose(px, EXPECTED["red"]),
      `hover switched preview: ${px}`,
    ).toBeTruthy();

    // ArrowRight steps to the next live image (yellow); Left steps back.
    await page.keyboard.press("ArrowRight");
    await page.waitForFunction(
      (i) => screenIdx === i && screenBmp,
      yellowIdx,
      { timeout: 15_000 },
    );
    px = await pixelAt(page, centre[0], centre[1]);
    expect(colorClose(px, EXPECTED["yellow"]), `got ${px}`).toBeTruthy();

    await page.keyboard.press("ArrowLeft");
    await page.waitForFunction(
      (i) => screenIdx === i && screenBmp,
      redIdx,
      { timeout: 15_000 },
    );

    // Escape exits fullscreen mode.
    await page.keyboard.press("Escape");
    await page.waitForFunction(() => !fsMode);
  });

  test("Delete trashes the selected file and removes its tile", async ({ page }) => {
    await openApp(page);
    await waitAllCached(page);
    const rects = await tileRects(page);
    const grayIdx = await page.evaluate(
      () => items.findIndex((it) => it.name === "gray.png"),
    );
    const rect = rects.find((r) => r.i === grayIdx);

    await page.mouse.click(rect.x + rect.w / 2, rect.y + rect.h / 2);
    await page.keyboard.press("Delete");

    // Optimistic: the tombstone applies synchronously on keypress — the
    // tile is gone from the UI before the server-side delete lands.
    const tombstonedEarly = await page.evaluate(
      (i) => items[i].removed,
      grayIdx,
    );
    expect(tombstonedEarly).toBe(true);

    // Focus scrolls to the survivor at the deletion point.
    const nextLive = await page.evaluate(
      (i) => items.findIndex((it, j) => j >= i && it && !it.removed),
      grayIdx,
    );
    expect(await page.evaluate(() => focus)).toBe(nextLive);

    // Server moves the file into .imagecropper-trash.
    const trashed = path.join(FIXTURE_DIR, ".imagecropper-trash", "gray.png");
    await expect
      .poll(() => fs.existsSync(trashed), { timeout: 10_000 })
      .toBe(true);
    expect(fs.existsSync(path.join(FIXTURE_DIR, "gray.png"))).toBe(false);

    // The tile disappears from the grid — the item stays as a tombstone
    // (removed:true) so positional ids remain stable for every client.
    await page.waitForFunction(
      () => items.every((it) => !it || it.name !== "gray.png" || it.removed),
      null,
      { timeout: 10_000 },
    );
  });

  test("m moves the selected file to the -m directory and removes its tile", async ({
    page,
  }) => {
    await openApp(page);
    await waitAllCached(page);
    const rects = await tileRects(page);
    const cyanIdx = await page.evaluate(
      () => items.findIndex((it) => it.name === "cyan.png"),
    );
    const rect = rects.find((r) => r.i === cyanIdx);

    await page.mouse.click(rect.x + rect.w / 2, rect.y + rect.h / 2);
    await page.keyboard.press("m");

    // Server moves the file into the -m directory.
    const moved = path.join(MOVE_DIR, "cyan.png");
    await expect
      .poll(() => fs.existsSync(moved), { timeout: 10_000 })
      .toBe(true);
    expect(fs.existsSync(path.join(FIXTURE_DIR, "cyan.png"))).toBe(false);

    // The tile disappears from the grid (tombstoned, like deletes).
    await page.waitForFunction(
      () => items.every((it) => !it || it.name !== "cyan.png" || it.removed),
      null,
      { timeout: 10_000 },
    );
  });

  test("a queued action is never timed out; an unacknowledged one is", async ({
    page,
  }) => {
    await openApp(page);
    await waitAllCached(page);

    // A delete the server has accepted into its queue ("action-queued") must
    // keep counting however slow the filesystem is — no local deadline.
    // One the server never acknowledged is dropped so the tile can be retried.
    await page.evaluate(() => {
      pendingActions.set(424242, {
        kind: "move",
        at: Date.now() - 300_000,
        queued: true,
        ids: [],
      });
      pendingActions.set(424243, {
        kind: "delete",
        at: Date.now() - 300_000,
        queued: false,
        ids: [],
      });
      scheduleDraw();
    });
    await expect(page.locator("#top")).toContainText("moving 1");
    await expect(page.locator("#top")).toContainText("deleting 1");

    // The watchdog only expires the unacknowledged dispatch.
    await expect(page.locator("#top")).not.toContainText("deleting", {
      timeout: 5_000,
    });
    await expect(page.locator("#status")).toContainText("not acknowledged");
    await expect(page.locator("#top")).toContainText("moving 1");
    expect(await page.evaluate(() => pendingActions.size)).toBe(1);
  });

  test("the server acknowledges a delete as queued", async ({ page }) => {
    await openApp(page);
    await waitAllCached(page);

    // Record `action-queued` events as they arrive.
    await page.evaluate(() => {
      window._acked = [];
      const orig = onData;
      window.onData = (f) => {
        if (f.type === EV_EVENT) {
          const ev = JSON.parse(new TextDecoder().decode(f.payload));
          if (ev.ev === "action-queued") window._acked.push(ev.tag);
        }
        return orig(f);
      };
    });

    // Dispatch a delete the server rejects (bogus signature) but whose
    // *enqueue* is acknowledged first — that ack is what keeps the client
    // from timing the op out.
    const tag = await page.evaluate(() => {
      const id = items.findIndex((it) => it && !it.removed);
      const tag = nextTag();
      dataWs.send(
        frame(
          CL_ACTION,
          0,
          str(JSON.stringify({ tag, kind: "delete", id, sig: "1" })),
        ),
      );
      return tag;
    });
    await page.waitForFunction(
      (t) => window._acked.includes(t),
      tag,
      { timeout: 10_000 },
    );
  });

  test("selection survives a renumbering resync and a manifest refresh", async ({
    page,
  }) => {
    await openApp(page);
    await waitAllCached(page);

    // Select two named tiles (ids are positional; names are the identity
    // the user cares about).
    const idx = (name) =>
      page.evaluate((n) => items.findIndex((it) => it && it.name === n), name);
    const redIdx = await idx("red.png");
    const purpleIdx = await idx("purple.png");
    const rects = await tileRects(page);
    const rectOf = (i) => rects.find((r) => r.i === i);

    await page.mouse.click(
      rectOf(redIdx).x + rectOf(redIdx).w / 2,
      rectOf(redIdx).y + rectOf(redIdx).h / 2,
    );
    await page.keyboard.down("Control");
    await page.mouse.click(
      rectOf(purpleIdx).x + rectOf(purpleIdx).w / 2,
      rectOf(purpleIdx).y + rectOf(purpleIdx).h / 2,
    );
    await page.keyboard.up("Control");
    await expect(page.locator("#top")).toContainText("2 selected");

    const selNames = () =>
      page.evaluate(() => [...selected].map((i) => items[i].name).sort());
    const focusName = () => page.evaluate(() => items[focus] && items[focus].name);
    expect(await selNames()).toEqual(["purple.png", "red.png"]);
    const focusBefore = await focusName();

    // Resync with EVERY id reassigned (reversed order) — same files. The
    // selection must follow the content, not the numbers.
    await page.evaluate(() => {
      const live = items.filter(Boolean).map((it) => ({ ...it }));
      const n = live.length;
      const renumbered = live.map((it, i) => ({ ...it, id: n - 1 - i }));
      applyPatch({
        resync: true,
        version: (manifest.version || 0) + 1,
        dirs_added: manifest.dirs.slice(),
        upsert: renumbered,
        remove: [],
      });
    });
    await page.waitForTimeout(200);
    expect(await selNames()).toEqual(["purple.png", "red.png"]);
    expect(await focusName()).toBe(focusBefore);
    await expect(page.locator("#top")).toContainText("2 selected");

    // A full manifest re-pull must preserve it too.
    await page.evaluate(() => refreshManifest());
    expect(await selNames()).toEqual(["purple.png", "red.png"]);
    expect(await focusName()).toBe(focusBefore);
    await expect(page.locator("#top")).toContainText("2 selected");
  });

  test("scrolling loads the tiles that come into view", async ({ page }) => {
    await openApp(page);
    await waitAllCached(page);

    // Make the album taller than the viewport, then drop every thumbnail so
    // the tiles below have to be fetched on demand.
    await page.evaluate(() => {
      tileH = 600;
      layout();
      thumbs.clear();
      pending.clear();
      scheduleDraw();
    });

    const visible = () =>
      page.evaluate(() =>
        tileRects()
          .filter((t) => t.y + t.h > scrollY && t.y < scrollY + canvas.height)
          .map((t) => ({ i: t.i, loaded: thumbs.has(t.i) })),
      );

    // Nothing loaded yet at the top of the album.
    await page.waitForFunction(
      () => {
        const vis = tileRects().filter(
          (t) => t.y + t.h > scrollY && t.y < scrollY + canvas.height,
        );
        return vis.length > 0 && vis.every((t) => thumbs.has(t.i));
      },
      null,
      { timeout: 15_000 },
    );

    // Jump to the last row and drop its thumbnails: the tiles that are now
    // in view are exactly the ones "not already loaded", so scrolling is what
    // has to fetch them.
    const target = await page.evaluate(() => {
      scrollY = rowY(rows.length - 1);
      const vis = tileRects().filter(
        (t) => t.y + t.h > scrollY && t.y < scrollY + canvas.height,
      );
      for (const t of vis) {
        thumbs.delete(t.i);
        pending.delete(t.i);
      }
      scheduleDraw();
      return { ids: vis.map((t) => t.i) };
    });
    expect(target.ids.length).toBeGreaterThan(0);
    expect(await page.evaluate((ids) => ids.every((i) => !thumbs.has(i)), target.ids))
      .toBe(true);

    await page.waitForFunction(
      () => {
        const vis = tileRects().filter(
          (t) => t.y + t.h > scrollY && t.y < scrollY + canvas.height,
        );
        return vis.length > 0 && vis.every((t) => thumbs.has(t.i));
      },
      null,
      { timeout: 15_000 },
    );
    const after = await visible();
    expect(after.every((t) => t.loaded)).toBe(true);
    // Every tile the scroll exposed got (re)loaded.
    expect(after.map((t) => t.i)).toEqual(expect.arrayContaining(target.ids));
  });
});
