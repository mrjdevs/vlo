(function() {
    const palette = ["#00f5ff", "#00ff88", "#ff6680", "#a78bfa", "#f472b6", "#ffaa00", "#06b6d4", "#84cc16"];

    function initCharts() {
        // ─── 1. BAR CHART ────────────────────────────────────────
        document.querySelectorAll('.vlo-chart-bar').forEach(chart => {
            const wrappers = Array.from(chart.querySelectorAll('.vlo-bar-wrapper'));
            if (wrappers.length === 0) return;
            
            // Read values from data-value attribute on wrapper
            const values = wrappers.map(w => parseFloat(w.dataset.value || 0));
            const max = Math.max(...values, 1);

            wrappers.forEach((wrapper, i) => {
                const fill = wrapper.querySelector('.vlo-bar-fill');
                if (!fill) return;
                
                const pct = (values[i] / max) * 100;
                fill.style.height = '0%';
                fill.style.backgroundColor = palette[i % palette.length];
                
                // Animate after a small delay
                setTimeout(() => { 
                    fill.style.height = `${pct}%`; 
                }, 50 + (i * 50));
            });
        });

        // ─── 2. HORIZONTAL BAR CHART ─────────────────────────────
        document.querySelectorAll('.vlo-chart-horizontal-bar').forEach(chart => {
            const bars = Array.from(chart.querySelectorAll('.vlo-bar-fill'));
            if (bars.length === 0) return;
            
            const values = bars.map(b => parseFloat(b.dataset.value || 0));
            const max = Math.max(...values, 1);

            bars.forEach((bar, i) => {
                const pct = (values[i] / max) * 100;
                bar.style.width = '0%';
                bar.style.backgroundColor = palette[i % palette.length];
                setTimeout(() => { bar.style.width = `${pct}%`; }, 50 + (i * 50));
            });
        });

        // ─── 3. LINE CHART ───────────────────────────────────────
        document.querySelectorAll('.vlo-chart-line').forEach(chart => {
            const svg = chart.querySelector('.vlo-line-svg');
            const path = chart.querySelector('.vlo-line-path');
            const area = chart.querySelector('.vlo-line-area');
            const points = Array.from(chart.querySelectorAll('.vlo-line-point'));
            const labelsBox = chart.querySelector('.vlo-line-labels');
            const tooltip = chart.querySelector('.vlo-line-tooltip'); //  Get tooltip
            
            if (points.length === 0) return;

            const data = points.map(p => ({ 
                value: parseFloat(p.dataset.value || 0), 
                label: p.dataset.label || '' 
            }));
            
            const minV = Math.min(...data.map(d => d.value));
            const maxV = Math.max(...data.map(d => d.value));
            const range = maxV - minV || 1;
            
            const W = 400, H = 200;
            const pL = 30, pR = 30, pT = 20, pB = 30;
            const dW = W - pL - pR;
            const dH = H - pT - pB;

            let lineD = '', areaD = '';
            const coords = [];

            data.forEach((d, i) => {
                const x = data.length === 1 ? W / 2 : pL + (i / (data.length - 1)) * dW;
                const normalizedY = (d.value - minV) / range;
                const y = pT + dH - (normalizedY * dH);
                
                coords.push({ x, y });

                if (i === 0) { 
                    lineD += `M ${x} ${y}`; 
                    areaD += `M ${x} ${H - pB} L ${x} ${y}`; 
                } else { 
                    lineD += ` L ${x} ${y}`; 
                    areaD += ` L ${x} ${y}`; 
                }
                
                points[i].setAttribute('cx', x);
                points[i].setAttribute('cy', y);

                // 🔥 Add Hover Events for Tooltip
                points[i].style.cursor = 'pointer';
                points[i].addEventListener('mouseenter', () => {
                    if (!tooltip) return;
                    // Format value to 2 decimal places
                    const formattedValue = d.value.toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 });
                    
                    tooltip.innerHTML = `
                        <div class="tt-label">${d.label}</div>
                        <div class="tt-value">${formattedValue}</div>
                    `;
                    
                    // Position tooltip exactly at the dot's percentage coordinates
                    tooltip.style.left = `${(x / W) * 100}%`;
                    tooltip.style.top = `${(y / H) * 100}%`;
                    tooltip.style.opacity = '1';
                });

                points[i].addEventListener('mouseleave', () => {
                    if (tooltip) tooltip.style.opacity = '0';
                });
            });

            if (data.length > 0) {
                areaD += ` L ${coords[coords.length - 1].x} ${H - pB} Z`;
            }

            if (path) path.setAttribute('d', lineD);
            if (area) area.setAttribute('d', areaD);

            if (labelsBox) {
                labelsBox.innerHTML = '';
                coords.forEach((c, i) => {
                    const span = document.createElement('span');
                    span.className = 'vlo-line-label';
                    span.textContent = data[i].label || '';
                    const leftPct = (c.x / W) * 100;
                    span.style.left = `${leftPct}%`;
                    labelsBox.appendChild(span);
                });
            }
        });

        // ─── 4. PIE CHART ────────────────────────────────────────
        document.querySelectorAll('.vlo-pie-svg').forEach(svg => {
            const slices = Array.from(svg.querySelectorAll('.vlo-pie-slice'));
            if (slices.length === 0) return;

            const total = slices.reduce((sum, s) => sum + parseFloat(s.dataset.value || 0), 0);
            let cumulativeAngle = 0;
            const cx = 100, cy = 100, radius = 80;

            slices.forEach((slice, i) => {
                const value = parseFloat(slice.dataset.value || 0);
                const sliceAngle = total > 0 ? (value / total) * 360 : 0;
                
                const startAngle = cumulativeAngle;
                const endAngle = cumulativeAngle + sliceAngle;
                cumulativeAngle = endAngle;

                const startRad = (startAngle - 90) * Math.PI / 180;
                const endRad = (endAngle - 90) * Math.PI / 180;

                const x1 = cx + radius * Math.cos(startRad);
                const y1 = cy + radius * Math.sin(startRad);
                const x2 = cx + radius * Math.cos(endRad);
                const y2 = cy + radius * Math.sin(endRad);

                const largeArc = sliceAngle > 180 ? 1 : 0;
                const pathStr = sliceAngle >= 359.99 
                    ? `M ${cx} ${cy} m -${radius} 0 a ${radius} ${radius} 0 1 1 ${radius * 2} 0 a ${radius} ${radius} 0 1 1 -${radius * 2} 0 Z`
                    : `M ${cx} ${cy} L ${x1} ${y1} A ${radius} ${radius} 0 ${largeArc} 1 ${x2} ${y2} Z`;

                slice.setAttribute('d', pathStr);
                slice.style.fill = palette[i % palette.length];
            });

            const legend = svg.closest('.vlo-chart-pie')?.querySelector('.vlo-pie-legend');
            if (legend) {
                Array.from(legend.querySelectorAll('.vlo-pie-legend-item')).forEach((item, i) => {
                    const dot = item.querySelector('.vlo-pie-legend-dot');
                    if (dot) dot.style.background = palette[i % palette.length];
                });
            }
        });

        // ─── 5. PROGRESS CHART ───────────────────────────────────
        document.querySelectorAll('.vlo-chart-progress').forEach(chart => {
            const bars = Array.from(chart.querySelectorAll('.vlo-bar-fill'));
            if (bars.length === 0) return;

            bars.forEach((bar, i) => {
                const targetWidth = bar.style.width || (bar.dataset.percentage + '%') || '0%';
                bar.style.width = '0%';
                bar.style.backgroundColor = palette[i % palette.length];
                setTimeout(() => { bar.style.width = targetWidth; }, 100 + (i * 100));
            });
        });

        // ─── 6. LOLLIPOP CHART ───────────────────────────────────
        document.querySelectorAll('.vlo-chart-lollipop').forEach(chart => {
            const sticks = Array.from(chart.querySelectorAll('.vlo-lollipop-stick'));
            if (sticks.length === 0) return;

            const values = sticks.map(s => parseFloat(s.dataset.value || 0));
            const max = Math.max(...values, 1);

            sticks.forEach((stick, i) => {
                const pct = (values[i] / max) * 100;
                stick.style.height = '0%';
                stick.style.backgroundColor = palette[i % palette.length];
                
                const dot = stick.querySelector('.vlo-lollipop-dot');
                if (dot) {
                    dot.style.backgroundColor = palette[i % palette.length];
                    dot.style.transform = 'translateY(100%)';
                }

                setTimeout(() => {
                    stick.style.height = `${pct}%`;
                    if (dot) dot.style.transform = 'translateY(0)';
                }, 50 + (i * 50));
            });
        });
    }

    // ─── INITIALIZATION ──────────────────────────────────────
    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', initCharts);
    } else {
        initCharts();
    }

    // ─── RE-INITIALIZE AFTER AJAX MUTATIONS ──────────────────
    window.addEventListener('vlo:mutation', () => {
        // Reset animation states so they can play again
        document.querySelectorAll('.vlo-bar-fill, .vlo-lollipop-stick').forEach(el => {
            el.style.height = '';
            el.style.width = '';
        });
        // Wait for DOM to settle, then redraw
        setTimeout(initCharts, 150);
    });
})();