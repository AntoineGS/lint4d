unit W1000MethodCall;

interface

type
  TDeprecatedMethod = class
    procedure OldMethod; deprecated;
  end;

procedure CallDeprecatedMethod(Value: TDeprecatedMethod);

implementation

procedure TDeprecatedMethod.OldMethod;
begin
end;

procedure CallDeprecatedMethod(Value: TDeprecatedMethod);
begin
  Value.OldMethod;
end;

end.
