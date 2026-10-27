document.addEventListener("click", function (event) {
    var trigger = event.target.closest("[data-callout-dismiss]");
    if (!trigger) return;
    event.preventDefault();
    var callout = trigger.closest(".vlo-callout");
    if (callout) callout.remove();
});
