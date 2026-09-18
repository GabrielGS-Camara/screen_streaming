export async function copyToClipboard(text, button) {
  try {
    await navigator.clipboard.writeText(text);
    const original = button.textContent;
    button.textContent = "Copiado!";
    setTimeout(() => {
      button.textContent = original;
    }, 1500);
  } catch (err) {
    console.error("clipboard write failed:", err);
  }
}
