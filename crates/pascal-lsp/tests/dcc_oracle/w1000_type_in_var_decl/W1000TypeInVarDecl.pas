unit W1000TypeInVarDecl;

interface

type
  TDeprecated = class
  end deprecated;

var
  GlobalValue: TDeprecated;

procedure UseDeprecatedType;

implementation

procedure UseDeprecatedType;
var
  LocalValue: TDeprecated;
begin
  GlobalValue := LocalValue;
end;

end.
