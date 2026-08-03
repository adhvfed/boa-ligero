// Publisher-neutral D1 resource-governor fixture.
//
// The 64 one-backedge functions run first so production OSR observes every
// reserved loop key without compiling it. The 192 distinct straight-line
// functions then cross the ordinary function-entry threshold. Backend time or
// payload breakers may deliberately stop compilation before all 192 artifacts
// are ready; the retained-state/resource counters describe that outcome.

function straightLineBody(additions) {
    let body = "";
    for (let index = 0; index < additions; index++) {
        body += "value = value + 1;";
    }
    return body + "return value;";
}

function main() {
    let sink = 0;

    const loopFunctions = Array.from({ length: 64 }, (_, index) =>
        Function(
            "limit",
            `let total = ${index}; for (let cursor = 0; cursor < limit; cursor++) total += cursor; return total;`,
        ),
    );
    for (let index = 0; index < loopFunctions.length; index++) {
        sink += loopFunctions[index](1);
    }

    const body = straightLineBody(50);
    const functions = Array.from({ length: 192 }, () => Function("value", body));
    for (let index = 0; index < functions.length; index++) {
        for (let repeat = 0; repeat < 64; repeat++) {
            sink += functions[index](0);
        }
    }

    return sink;
}
