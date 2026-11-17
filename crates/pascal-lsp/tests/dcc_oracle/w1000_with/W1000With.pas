unit W1000With;

interface

type
  TDeprecatedMember = class
  public
    DeprecatedField: Integer deprecated;
  end;

procedure UseWith(Value: TDeprecatedMember);

implementation

procedure UseWith(Value: TDeprecatedMember);
begin
  with Value do
    DeprecatedField := 1;
end;

end.
