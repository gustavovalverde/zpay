export interface PaymentFact {
  label: string;
  value: string;
}

interface PaymentFactListProps {
  facts: PaymentFact[];
}

export function PaymentFactList({ facts }: PaymentFactListProps) {
  return (
    <dl className="payment-fact-list">
      {facts.map((fact) => (
        <div key={fact.label}>
          <dt>{fact.label}</dt>
          <dd>{fact.value}</dd>
        </div>
      ))}
    </dl>
  );
}
