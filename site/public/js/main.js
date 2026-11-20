// Reveal sections as they scroll in, size the benchmark bars, and respect reduced motion.
document.documentElement.classList.add('js');

const reduce = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
if (reduce) {
  // SMIL orbits ignore CSS, so pause them directly.
  document.querySelectorAll('svg.orbit').forEach((s) => s.pauseAnimations());
}

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
