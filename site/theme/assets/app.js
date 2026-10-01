// Server-switching scene, hotbar number keys and copy buttons.
(() => {
  const scene = document.querySelector('[data-scene]');
  const chat = document.querySelector('[data-chat]');
  if (scene && chat) {
    const names = [...scene.querySelectorAll('.island .sign')].map((s) => s.textContent);
    const log = chat.querySelector('.log');
    const hop = scene.querySelector('.hop');
    let at = 0;
    const say = (html) => {
      const li = document.createElement('li');
      li.innerHTML = html;
      log.append(li);
      while (log.children.length > 4) log.firstElementChild.remove();
    };
    chat.querySelectorAll('[data-go]').forEach((button) => button.addEventListener('click', () => {
      const to = Number(button.dataset.go);
      say(`<span class="gray">&gt;</span> ${button.textContent}`);
      if (to === at) { say(`<span class="gray">Already on</span> <span class="gold">${names[to]}</span>`); return; }
      at = to;
      scene.style.setProperty('--i', to);
      hop.classList.remove('go'); void hop.offsetWidth; hop.classList.add('go');
      say(`<span class="green">Moved to ${names[to]}</span> <span class="gray">on the same connection</span>`);
    }));
  }

  // Number keys 1 to 9 select a hotbar slot, as in the game.
  document.addEventListener('keydown', (event) => {
    if (event.ctrlKey || event.metaKey || event.altKey) return;
    if (event.target.closest('input, textarea, select, [contenteditable]')) return;
    const slot = document.querySelector(`.hotbar a[data-key="${event.key}"]`);
    if (slot) slot.click();
  });

  document.querySelectorAll('.code').forEach((figure) => {
    const button = document.createElement('button');
    button.className = 'copy';
    button.type = 'button';
    button.textContent = 'Copy';
    button.addEventListener('click', async () => {
      try {
        await navigator.clipboard.writeText(figure.querySelector('pre').innerText);
        button.textContent = 'Copied';
      } catch {
        button.textContent = 'Copy failed';
      }
      setTimeout(() => { button.textContent = 'Copy'; }, 1500);
    });
    figure.append(button);
  });
})();
