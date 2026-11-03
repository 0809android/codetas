import { t } from "./i18n";

/**
 * In-page replacement for `window.confirm`. The desktop webview has no
 * JavaScript dialog delegate, so `window.confirm` returns false without ever
 * showing a prompt and every action behind it silently does nothing.
 */
export function confirmDialog(message: string): Promise<boolean> {
  return new Promise((resolve) => {
    const previousFocus = document.activeElement as HTMLElement | null;
    const scrim = document.createElement("div");
    scrim.className = "confirmation-scrim";
    const dialog = document.createElement("section");
    dialog.className = "confirmation-dialog";
    dialog.setAttribute("role", "dialog");
    dialog.setAttribute("aria-modal", "true");
    const text = document.createElement("p");
    text.className = "confirmation-message";
    text.textContent = message;
    const actions = document.createElement("div");
    actions.className = "confirmation-actions";
    const cancel = document.createElement("button");
    cancel.type = "button";
    cancel.className = "secondary";
    cancel.textContent = t("confirm.cancel");
    const accept = document.createElement("button");
    accept.type = "button";
    accept.className = "primary";
    accept.textContent = t("confirm.proceed");
    actions.append(cancel, accept);
    dialog.append(text, actions);
    scrim.append(dialog);

    const finish = (result: boolean) => {
      document.removeEventListener("keydown", onKeydown, true);
      scrim.remove();
      previousFocus?.focus?.({ preventScroll: true });
      resolve(result);
    };
    const onKeydown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        finish(false);
      }
    };
    cancel.addEventListener("click", () => finish(false));
    accept.addEventListener("click", () => finish(true));
    scrim.addEventListener("click", (event) => {
      if (event.target === scrim) finish(false);
    });
    document.addEventListener("keydown", onKeydown, true);
    document.body.append(scrim);
    accept.focus({ preventScroll: true });
  });
}
