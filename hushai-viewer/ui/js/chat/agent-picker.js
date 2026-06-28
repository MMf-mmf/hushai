// The agent selector strip. One agent ships today; the loop is already N-ready so adding
// agents later is purely a backend registry change + this rendering one tab per agent.

export function renderAgentPicker(container, agents, activeId, onSelect) {
  container.innerHTML = "";
  for (const a of agents) {
    const tab = document.createElement("button");
    tab.type = "button";
    tab.className = "agent-tab" + (a.id === activeId ? " active" : "");
    tab.textContent = a.name;
    tab.title = a.description || "";
    tab.addEventListener("click", () => onSelect(a.id));
    container.appendChild(tab);
  }
}
