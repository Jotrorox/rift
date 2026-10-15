// Server-switching demo, metric reveal, navigation chrome and copy buttons.
(() => {
  const reduceMotion = matchMedia('(prefers-reduced-motion: reduce)');

  // A damped spring described by damping ratio and response (seconds), as in
  // UIKit. Retargeting keeps the current value and velocity, so any motion
  // can be interrupted and redirected without a jump.
  class Spring {
    constructor(value, onUpdate) {
      this.value = value;
      this.velocity = 0;
      this.target = value;
      this.onUpdate = onUpdate;
      this.frame = 0;
    }

    set(value) {
      this.stop();
      this.value = this.target = value;
      this.velocity = 0;
      this.onUpdate(value);
    }

    stop() {
      cancelAnimationFrame(this.frame);
      this.frame = 0;
    }

    to(target, { damping = 1, response = 0.35, velocity = this.velocity } = {}) {
      if (reduceMotion.matches) { this.set(target); return; }
      this.target = target;
      this.velocity = velocity;
      const stiffness = (2 * Math.PI / response) ** 2;
      const friction = 4 * Math.PI * damping / response;
      let last = performance.now();
      cancelAnimationFrame(this.frame);
      const tick = (now) => {
        let dt = Math.min((now - last) / 1000, 1 / 30);
        last = now;
        while (dt > 0) {
          const h = Math.min(dt, 1 / 240);
          this.velocity += (-stiffness * (this.value - this.target) - friction * this.velocity) * h;
          this.value += this.velocity * h;
          dt -= h;
        }
        if (Math.abs(this.velocity) < 2 && Math.abs(this.value - this.target) < 0.2) {
          this.value = this.target;
          this.velocity = 0;
          this.frame = 0;
        } else {
          this.frame = requestAnimationFrame(tick);
        }
        this.onUpdate(this.value);
      };
      this.frame = requestAnimationFrame(tick);
    }
  }

  // Where a flick would come to rest, using scroll-view deceleration.
  const project = (velocity, rate = 0.995) => (velocity / 1000) * rate / (1 - rate);

  // Progressive resistance past an edge instead of a hard stop.
  const rubberband = (overshoot, dimension, c = 0.55) =>
    (overshoot * dimension * c) / (dimension + c * Math.abs(overshoot));

  const seg = document.querySelector('[data-seg]');
  const term = document.querySelector('[data-term]');
  if (seg && term) {
    const buttons = [...seg.querySelectorAll('[role="radio"]')];
    const thumb = seg.querySelector('.seg-thumb');
    const cmdLine = term.querySelector('[data-line="cmd"]');
    const world = term.querySelector('[data-world]');
    const n = buttons.length;
    let width = 0;
    let index = 0;
    let under = -1;

    const measure = () => { width = (seg.clientWidth - 4) / n; };
    const render = (x) => {
      thumb.style.transform = `translate3d(${x}px, 0, 0)`;
      const i = Math.max(0, Math.min(n - 1, Math.round(x / (width || 1))));
      if (i !== under) {
        buttons[under]?.removeAttribute('data-under');
        buttons[i].setAttribute('data-under', '');
        under = i;
      }
    };
    const spring = new Spring(0, render);

    const show = (i) => {
      cmdLine.textContent = `› ${buttons[i].dataset.cmd}`;
      world.textContent = buttons[i].textContent;
      term.setAttribute('data-swap', '');
      void term.offsetWidth;
      term.removeAttribute('data-swap');
    };

    const select = (i, motion = {}) => {
      i = Math.max(0, Math.min(n - 1, i));
      spring.to(i * width, { damping: 1, response: 0.35, ...motion });
      if (i === index) return;
      buttons[index].setAttribute('aria-checked', 'false');
      buttons[index].tabIndex = -1;
      buttons[i].setAttribute('aria-checked', 'true');
      buttons[i].tabIndex = 0;
      index = i;
      show(i);
    };

    measure();
    render(0);
    new ResizeObserver(() => {
      measure();
      if (!spring.frame) spring.set(index * width);
    }).observe(seg);

    // Pointer: a tap selects a segment; grabbing the thumb drags it 1:1 and a
    // release throws it toward the segment its momentum points at.
    let drag = null;
    seg.addEventListener('pointerdown', (event) => {
      if (event.button !== 0) return;
      const left = seg.getBoundingClientRect().left + 2;
      const x = spring.value; // presentation value, so a moving thumb can be caught
      const onThumb = event.clientX - left >= x && event.clientX - left <= x + width;
      drag = { id: event.pointerId, startX: event.clientX, from: x, onThumb, moved: false, samples: [] };
      seg.setPointerCapture(event.pointerId);
      if (onThumb) {
        spring.stop();
        seg.setAttribute('data-pressed', '');
      }
    });

    seg.addEventListener('pointermove', (event) => {
      if (!drag || event.pointerId !== drag.id) return;
      const dx = event.clientX - drag.startX;
      if (!drag.moved && Math.abs(dx) < 4) return;
      drag.moved = true;
      if (!drag.onThumb) return;
      seg.setAttribute('data-dragging', '');
      const max = (n - 1) * width;
      let x = drag.from + dx;
      if (x < 0) x = rubberband(x, width);
      else if (x > max) x = max + rubberband(x - max, width);
      spring.value = x;
      spring.velocity = 0;
      render(x);
      drag.samples.push({ x, t: event.timeStamp });
      while (drag.samples.length > 2 && event.timeStamp - drag.samples[0].t > 100) drag.samples.shift();
    });

    const release = (event) => {
      if (!drag || event.pointerId !== drag.id) return;
      const d = drag;
      drag = null;
      seg.removeAttribute('data-pressed');
      seg.removeAttribute('data-dragging');
      if (!d.moved) {
        // Pointer capture retargets the event, so find the segment by position.
        select(Math.floor((event.clientX - seg.getBoundingClientRect().left - 2) / width), { velocity: 0 });
        return;
      }
      if (!d.onThumb) { spring.to(index * width); return; }
      const first = d.samples[0];
      const last = d.samples[d.samples.length - 1];
      const velocity = first && last && last.t > first.t ? (last.x - first.x) / ((last.t - first.t) / 1000) : 0;
      const rest = spring.value + project(velocity);
      const target = Math.max(0, Math.min(n - 1, Math.round(rest / width)));
      const thrown = Math.abs(velocity) > 300;
      select(target, { velocity, damping: thrown ? 0.8 : 1, response: 0.3 });
    };
    seg.addEventListener('pointerup', release);
    seg.addEventListener('pointercancel', release);

    // Keyboard: radio-group arrows; Space and Enter arrive as clicks.
    seg.addEventListener('keydown', (event) => {
      const step = { ArrowRight: 1, ArrowDown: 1, ArrowLeft: -1, ArrowUp: -1 }[event.key];
      let to = step === undefined ? null : (index + step + n) % n;
      if (event.key === 'Home') to = 0;
      if (event.key === 'End') to = n - 1;
      if (to === null) return;
      event.preventDefault();
      select(to);
      buttons[to].focus();
    });
    buttons.forEach((button, i) => button.addEventListener('click', (event) => {
      if (event.detail === 0) select(i);
    }));
  }

  // Metric bars grow when they first scroll into view.
  const perf = document.querySelector('.perf[data-reveal]');
  if (perf && !reduceMotion.matches && 'IntersectionObserver' in window) {
    if (perf.getBoundingClientRect().top > innerHeight * 0.85) {
      perf.dataset.reveal = 'pending';
      const io = new IntersectionObserver((entries) => {
        if (!entries.some((e) => e.isIntersecting)) return;
        requestAnimationFrame(() => perf.removeAttribute('data-reveal'));
        io.disconnect();
      }, { rootMargin: '0px 0px -20% 0px' });
      io.observe(perf);
    }
  }

  // The bar only draws its edge once content actually runs underneath it.
  const nav = document.querySelector('[data-nav]');
  if (nav) {
    const update = () => nav.toggleAttribute('data-scrolled', scrollY > 2);
    addEventListener('scroll', update, { passive: true });
    update();
  }

  // Documentation sidebar: always open on wide screens, a menu on narrow ones.
  const side = document.querySelector('[data-side]');
  if (side) {
    const wide = matchMedia('(min-width: 901px)');
    const apply = () => { side.open = wide.matches; };
    wide.addEventListener('change', apply);
    apply();
  }

  // Highlight the section being read in the "On this page" column.
  const tocLinks = [...document.querySelectorAll('.toc a')];
  if (tocLinks.length) {
    const headings = tocLinks.map((a) => document.getElementById(decodeURIComponent(a.hash.slice(1)))).filter(Boolean);
    let ticking = false;
    const spy = () => {
      ticking = false;
      let active = 0;
      headings.forEach((h, i) => { if (h.getBoundingClientRect().top < 120) active = i; });
      tocLinks.forEach((a, i) => a.toggleAttribute('data-active', i === active));
    };
    addEventListener('scroll', () => { if (!ticking) { ticking = true; requestAnimationFrame(spy); } }, { passive: true });
    spy();
  }

  document.querySelectorAll('.code').forEach((figure) => {
    const button = document.createElement('button');
    button.className = 'copy';
    button.type = 'button';
    button.textContent = 'Copy';
    let timer;
    button.addEventListener('click', async () => {
      try {
        await navigator.clipboard.writeText(figure.querySelector('pre').innerText);
        button.textContent = 'Copied';
      } catch {
        button.textContent = 'Copy failed';
      }
      button.setAttribute('data-done', '');
      clearTimeout(timer);
      timer = setTimeout(() => { button.textContent = 'Copy'; button.removeAttribute('data-done'); }, 1500);
    });
    figure.append(button);
  });
})();
