import { expect, test, type Page } from "@playwright/test";

const CONFIG = "settings:\n  workers: 2\n";

async function stubBackend(page: Page) {
  const posts: string[] = [];
  await page.route("**/ui/session", (r) => r.fulfill({ status: 200, body: "ok" }));
  await page.route("**/ws/**", (r) => r.abort());
  await page.route("**/api/config", async (r) => {
    if (r.request().method() === "POST") {
      posts.push(r.request().postData() ?? "");
      return r.fulfill({ status: 200, contentType: "application/json", body: '{"revision":4}' });
    }
    return r.fulfill({ status: 200, headers: { "x-config-revision": "3" }, body: CONFIG });
  });
  return posts;
}

test("Settings submit asks for confirmation before posting the config", async ({ page }) => {
  const posts = await stubBackend(page);
  await page.goto("/settings");

  await page.getByRole("button", { name: "Submit as a new revision" }).click();
  const dialog = page.getByRole("dialog");
  await expect(dialog).toContainText("Apply this configuration?");
  expect(posts).toEqual([]);

  await dialog.getByRole("button", { name: "Cancel" }).click();
  await expect(dialog).toBeHidden();
  expect(posts).toEqual([]);

  await page.getByRole("button", { name: "Submit as a new revision" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Apply" }).click();
  await expect(page.getByText("accepted as revision 4")).toBeVisible();
  expect(posts).toEqual([CONFIG]);
});
