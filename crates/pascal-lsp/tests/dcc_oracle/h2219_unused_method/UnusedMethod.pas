unit UnusedMethod;

interface

type
  TFoo = class
  private
    procedure Unused;
  end;

implementation

procedure TFoo.Unused;
begin
end;

end.
