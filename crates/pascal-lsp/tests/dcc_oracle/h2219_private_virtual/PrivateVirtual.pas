unit PrivateVirtual;

interface

type
  TFoo = class
  private
    procedure UncalledVirtual; virtual;
  end;

implementation

procedure TFoo.UncalledVirtual;
begin
end;

end.
