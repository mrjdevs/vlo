document.addEventListener('DOMContentLoaded', () => {
    const palette = ["#00f5ff", "#00ff88", "#ff6680", "#a78bfa", "#f472b6", "#ffaa00", "#06b6d4", "#84cc16"];

    // --- PIE CHART LOGIC ---
    document.querySelectorAll('.vlo-pie-svg').forEach(svg => {
        const slices = Array.from(svg.querySelectorAll('.vlo-pie-slice'));
        if (slices.length === 0) return;

        const total = slices.reduce((sum, slice) => sum + parseFloat(slice.dataset.value || 0), 0);
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
            const path = sliceAngle >= 359.99 
                ? `M ${cx} ${cy} m -${radius} 0 a ${radius} ${radius} 0 1 1 ${radius * 2} 0 a ${radius} ${radius} 0 1 1 -${radius * 2} 0 Z`
                : `M ${cx} ${cy} L ${x1} ${y1} A ${radius} ${radius} 0 ${largeArc} 1 ${x2} ${y2} Z`;

            slice.setAttribute('d', path);
            slice.setAttribute('fill', palette[i % palette.length]);
            slice.classList.add('vlo-pie-slice-animated');
        });

        const legend = svg.closest('.vlo-chart-pie')?.querySelector('.vlo-pie-legend');
        if (legend) {
            Array.from(legend.querySelectorAll('.vlo-pie-legend-item')).forEach((item, i) => {
                const dot = item.querySelector('.vlo-pie-legend-dot');
                if (dot) dot.style.background = palette[i % palette.length];
            });
        }
    });

    // --- LINE CHART LOGIC ---
    document.querySelectorAll('.vlo-line-svg').forEach(svg => {
        const points = Array.from(svg.querySelectorAll('.vlo-line-point'));
        if (points.length === 0) return;

        const values = points.map(p => parseFloat(p.dataset.value || 0));
        const max = Math.max(...values, 1); // Avoid division by zero
        const len = points.length;
        const pathPoints = [];

        points.forEach((point, i) => {
            // Calculate X (spread across 380px with 10px padding)
            const x = len > 1 ? (i / (len - 1)) * 380 + 10 : 200;
            // Calculate Y (inverted: 190 is bottom, 10 is top)
            const y = 190 - (values[i] / max) * 180;
            
            point.setAttribute('cx', x);
            point.setAttribute('cy', y);
            pathPoints.push(`${x} ${y}`);
        });

        // Draw the connecting line
        const path = svg.querySelector('.vlo-line-path');
        if (path && pathPoints.length > 0) {
            const d = pathPoints.length === 1 
                ? `M ${pathPoints[0]}` 
                : `M ${pathPoints[0]} L ${pathPoints.slice(1).join(' L ')}`;
            path.setAttribute('d', d);
        }
    });
});