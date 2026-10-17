import { CircuitBreaker } from "./circuit";

describe("circuit breaker", () => {
  it("opens the circuit breaker after five failures", () => {
    const breaker = new CircuitBreaker(5);
    for (let i = 0; i < 5; i++) breaker.fail();
    expect(breaker.isOpen()).toBe(true);
  });

  it("lets the circuit breaker close after one probe", () => {
    const breaker = new CircuitBreaker(1);
    breaker.fail();
    expect(breaker.probe()).toBe("closed"); // circuit breaker
  });
});