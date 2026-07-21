import { test, expect } from "./helpers";

const ENCRYPTING = {
  encrypting: true,
  latest_encrypted_at: 1784642252,
  latest_plain_at: 1784642100,
  encrypted_clip_count: 3,
};

const PLAIN_NEWEST = {
  encrypting: false,
  latest_encrypted_at: 1784642252,
  latest_plain_at: 1784642400,
  encrypted_clip_count: 3,
};

test.describe("storage encryption banner", () => {
  test("shows warning banner when dashcam encryption is currently active", async ({
    page,
    probe,
  }) => {
    await page.route("**/api/recording/encryption", (r) =>
      r.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify(ENCRYPTING),
      }),
    );

    await page.goto("/storage", { waitUntil: "load" });
    const banner = page.locator("#storage-encryption-banner");
    await expect(banner).toBeVisible();
    await expect(banner).toContainText("Dashcam encryption is on");
    await expect(banner).toContainText("Encrypt Dashcam Recordings");

    expect(probe.pageErrors, `pageerror(s): ${JSON.stringify(probe.pageErrors)}`).toEqual([]);
    expect(
      probe.consoleErrors,
      `console error(s): ${JSON.stringify(probe.consoleErrors)}`,
    ).toEqual([]);
  });

  test("hides warning banner when newest clip is plain", async ({ page }) => {
    await page.route("**/api/recording/encryption", (r) =>
      r.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify(PLAIN_NEWEST),
      }),
    );

    await page.goto("/storage", { waitUntil: "load" });
    await expect(page.locator("#storage-encryption-banner")).toHaveCount(0);
  });
});
