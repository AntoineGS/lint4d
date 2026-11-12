unit AbstractClass;
interface
type
  TObject = class
  end;
  IRequired = interface(IInterface)
    procedure Required;
  end;
  TAbstract = class abstract(TObject, IRequired)
  end;
implementation
end.
