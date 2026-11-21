// Reveal sections as they scroll in, size the benchmark bars, and respect reduced motion.
document.documentElement.classList.add('js');

const reduce = window.matchMedia('(prefers-reduced-motion: reduce)').matches;

// Bar width = result / reference, set as a custom property (allowed under the CSP, unlike inline styles).
document.querySelectorAll('.bar').forEach((bar) => {
  const pct = Math.min(100, (Number(bar.dataset.value) / Number(bar.dataset.ref)) * 100);
  bar.style.setProperty('--w', `${pct.toFixed(1)}%`);
});

const sections = document.querySelectorAll('.reveal');
if (reduce || !('IntersectionObserver' in window)) {
  sections.forEach((el) => el.classList.add('in'));
} else {
  const io = new IntersectionObserver((entries) => {
    for (const e of entries) {
      if (e.isIntersecting) {
        e.target.classList.add('in');
        io.unobserve(e.target);
      }
    }
  }, { threshold: 0.15 });
  sections.forEach((el) => io.observe(el));
}

// Downloads: point the hero button at this visitor's system. Windows and Linux visitors get
// a tab for their system (the other is one click away); anyone else, or without JS, sees
// every download.
const ua = navigator.userAgent;
const platform = (navigator.userAgentData && navigator.userAgentData.platform) || navigator.platform || '';
function detectOs() {
  // Android and ChromeOS report Linux but can't run Flash.
  if (/Android|CrOS/i.test(ua)) return null;
  if (/Win/i.test(platform) || /Windows NT/.test(ua)) return 'windows';
  if (/Linux/i.test(platform) || /Linux|X11/.test(ua)) return 'linux';
  return null;
}
const os = detectOs();
const hero = document.getElementById('hero-download');
const tabs = document.querySelector('.os-tabs');
const groups = document.querySelectorAll('.os-group');
const download = document.getElementById('download');

function show(which) {
  groups.forEach((g) => g.classList.toggle('is-hidden', g.dataset.os !== which));
  tabs.querySelectorAll('button').forEach((b) => {
    const on = b.dataset.os === which;
    b.setAttribute('aria-selected', String(on));
    b.tabIndex = on ? 0 : -1;
  });
}

if (os && hero && tabs && download) {
  hero.textContent = os === 'windows' ? 'Download for Windows' : 'Download for Linux';
  // Land on the section heading, so the system tabs are in view.
  hero.href = "#download";
  tabs.hidden = false;
  download.classList.add('tabbed');
  show(os);
  tabs.addEventListener('click', (e) => {
    const b = e.target.closest('button[data-os]');
    if (b) show(b.dataset.os);
  });
  // Arrow keys move between the tabs, as for any tab list.
  tabs.addEventListener('keydown', (e) => {
    if (e.key !== 'ArrowLeft' && e.key !== 'ArrowRight') return;
    const next = tabs.querySelector('button[aria-selected="false"]');
    if (next) {
      show(next.dataset.os);
      next.focus();
    }
  });
  // The hero button opens the downloads with this system's tab picked.
  hero.addEventListener('click', () => show(os));
}

// On Linux, recommend the package for the distribution the browser names, if it does.
if (os === 'linux') {
  const distro = /Ubuntu|Debian|Mint/i.test(ua) ? 'debian' : /Fedora|Red Hat|openSUSE|SUSE/i.test(ua) ? 'fedora' : null;
  const pick = distro && document.querySelector(`.dl[data-distro="${distro}"]`);
  if (pick) {
    document.querySelectorAll('#dl-linux .dl.rec').forEach((el) => el.classList.remove('rec'));
    pick.classList.add('rec');
  }
}
